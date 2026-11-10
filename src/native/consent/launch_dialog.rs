use super::record::{applied, authorized, authorized_with};
use super::store::{BypassStart, Environment, LaunchTrust, Trust, user_home};
use crate::native::clock::{Clock, SystemClock};
use crate::native::delivery::verify_terminal_surface_ownership_until;
use crate::native::session::{Reader, schema::SessionManifest};
use crate::native::{provider, terminal};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

fn respond_to_workspace_dialog(
    directory: &Path,
    session: &terminal::TerminalSession,
    input: &terminal::GuardedDialogInput,
    dispatch: impl FnOnce(&terminal::GuardedDialogInput) -> Result<bool>,
) -> Result<bool> {
    if !terminal::guards_dialog_input(session.kind) {
        bail!(
            "the workspace trust dialog cannot be answered with a guarded key in {}; no key was sent; use the provider's own workspace trust prompt, then launch again",
            session.kind.display_name()
        );
    }
    applied(directory, "workspace-dialog-response-pending")?;
    dispatch(input)
}

pub(in crate::native) fn complete_launch(
    directory: &Path,
    target: FirstPartyCli,
    session: &terminal::TerminalSession,
    expected_claim: &str,
    deadline: Instant,
) -> Result<()> {
    complete_launch_with(
        provider::launch_trust(target),
        || {
            require_launch_dialog_live(directory, false)?;
            authorized(directory, target)
        },
        || Reader::open_unchecked(directory).manifest(),
        |signal| signal(directory),
        |manifest, bypass_start| {
            let id = manifest.id.clone();
            LaunchDialog {
                directory,
                target,
                session,
                expected_claim,
                deadline,
                manifest,
                bypass_start,
            }
            .complete(
                &Environment::current(),
                &mut SystemClock,
                &mut TerminalDialogSurface {
                    directory,
                    id: &id,
                    session,
                },
            )
        },
    )
}

fn complete_launch_with(
    facts: LaunchTrust,
    authorized: impl FnOnce() -> Result<bool>,
    manifest: impl FnOnce() -> Result<SessionManifest>,
    started: impl FnOnce(BypassStart) -> Result<Option<bool>>,
    dialog: impl FnOnce(SessionManifest, Option<BypassStart>) -> Result<()>,
) -> Result<()> {
    if !facts.answers_launch_dialog || !authorized()? {
        return Ok(());
    }
    let manifest = manifest()?;
    // Under bypass the provider may show no dialog, and then its trust store never changes:
    // only its own start report ends the wait. A launch without that report keeps the
    // dialog unanswered, as before the report existed.
    let bypass_start = match facts.bypass_start {
        Some(signal) if manifest.yolo => match started(signal)? {
            None | Some(true) => return Ok(()),
            Some(false) => Some(signal),
        },
        _ => None,
    };
    dialog(manifest, bypass_start)
}

pub(super) trait DialogSurface {
    fn screen(&mut self, deadline: Instant) -> Result<String>;
    fn answer(&mut self, input: &terminal::GuardedDialogInput, deadline: Instant) -> Result<bool>;
}

struct TerminalDialogSurface<'a> {
    directory: &'a Path,
    id: &'a str,
    session: &'a terminal::TerminalSession,
}

impl DialogSurface for TerminalDialogSurface<'_> {
    fn screen(&mut self, deadline: Instant) -> Result<String> {
        verify_terminal_surface_ownership_until(
            self.directory,
            self.id,
            self.session,
            deadline,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        terminal::read_screen(self.session, deadline)
    }

    fn answer(&mut self, input: &terminal::GuardedDialogInput, deadline: Instant) -> Result<bool> {
        terminal::guarded_dialog_input(self.directory, self.session, input, deadline)
    }
}

pub(super) struct LaunchDialog<'a> {
    pub(super) directory: &'a Path,
    pub(super) target: FirstPartyCli,
    pub(super) session: &'a terminal::TerminalSession,
    pub(super) expected_claim: &'a str,
    pub(super) deadline: Instant,
    pub(super) manifest: SessionManifest,
    // Set only for a launch under bypass whose provider reports its own start.
    pub(super) bypass_start: Option<BypassStart>,
}

fn require_launch_dialog_live(directory: &Path, responded: bool) -> Result<()> {
    let status = Reader::open_unchecked(directory)
        .status()
        .map_err(|error| launch_dialog_error(error, responded))?;
    if status.state.ended() {
        let detail = if responded {
            "after the one workspace trust response; the provider may have taken it as No"
        } else {
            "before the workspace trust response"
        };
        return Err(launch_dialog_error(
            format!("the session is {} {detail}", status.state),
            responded,
        ));
    }
    Ok(())
}

impl LaunchDialog<'_> {
    pub(super) fn complete(
        &self,
        env: &Environment,
        clock: &mut impl Clock,
        surface: &mut dyn DialogSurface,
    ) -> Result<()> {
        let Self {
            directory,
            target,
            session,
            deadline,
            manifest,
            ..
        } = self;
        let (target, deadline) = (*target, *deadline);
        user_home(env)?;
        let expires_unix_ms = crate::native::session::primitives::unix_ms()
            + deadline.saturating_duration_since(clock.now()).as_millis();
        let settle = provider::launch_trust(target).dialog_settle;
        let mut stable: Option<(String, Instant)> = None;
        let mut responded = false;
        loop {
            require_launch_dialog_live(directory, responded)?;
            let trusted = matches!(
                provider::workspace_trust(target, &manifest.workspace, env, None)
                    .map_err(|error| launch_dialog_error(error, responded))?,
                Trust::Trusted(_)
            );
            require_launch_dialog_live(directory, responded)?;
            if trusted {
                if responded {
                    applied(directory, "provider-workspace-dialog")
                        .context("provider trust is confirmed; recording the Bridge applied marker failed; no input was retried")?;
                }
                return Ok(());
            }
            if clock.now() >= deadline {
                return Err(launch_dialog_error(
                    "workspace trust was not confirmed before the deadline; inspect workspace_consent and the managed terminal",
                    responded,
                ));
            }
            if !responded {
                // The provider started without the dialog: nothing to answer, and its
                // store will not change. After a response only the store confirms trust.
                if let Some(signal) = self.bypass_start
                    && signal(directory).map_err(|error| launch_dialog_error(error, false))?
                        == Some(true)
                {
                    return Ok(());
                }
                if !authorized_with(directory, target, env)
                    .map_err(|error| launch_dialog_error(error, false))?
                {
                    bail!(
                        "workspace consent changed before trust dialog response; no response sent"
                    );
                }
                let screen = surface
                    .screen(deadline)
                    .map_err(|error| launch_dialog_error(error, false))?;
                let observed_at = clock.now();
                require_launch_dialog_live(directory, false)?;
                if let Some(key) =
                    provider::workspace_trust_key(target, &screen, &manifest.workspace)
                {
                    let since = match &stable {
                        Some((previous, since)) if previous == &screen => *since,
                        _ => {
                            stable = Some((screen.clone(), observed_at));
                            observed_at
                        }
                    };
                    if observed_at.saturating_duration_since(since) < settle {
                        let remaining = deadline.saturating_duration_since(clock.now());
                        clock.sleep(Duration::from_millis(200).min(remaining));
                        continue;
                    }
                    if !authorized_with(directory, target, env)
                        .map_err(|error| launch_dialog_error(error, false))?
                    {
                        bail!(
                            "workspace consent changed before trust dialog response; no response sent"
                        );
                    }
                    require_launch_dialog_live(directory, false)?;
                    if clock.now() >= deadline {
                        continue;
                    }
                    let mut attempted = false;
                    responded = respond_to_workspace_dialog(
                        directory,
                        session,
                        &terminal::GuardedDialogInput {
                            screen,
                            key,
                            expected_claim: Some(self.expected_claim.to_owned()),
                            expires_unix_ms: Some(expires_unix_ms),
                        },
                        |input| {
                            require_launch_dialog_live(directory, false)?;
                            if clock.now() >= deadline {
                                bail!("workspace trust response deadline expired before dispatch");
                            }
                            attempted = true;
                            surface.answer(input, deadline)
                        },
                    )
                    .map_err(|error| {
                        let attempted = attempted && !error.is::<terminal::StaleDialogClaim>();
                        launch_dialog_error(error, attempted)
                    })?;
                    if !responded {
                        stable = None;
                    }
                    require_launch_dialog_live(directory, responded)?;
                } else {
                    stable = None;
                }
            }
            let remaining = deadline.saturating_duration_since(clock.now());
            clock.sleep(Duration::from_millis(200).min(remaining));
        }
    }
}

fn launch_dialog_error(error: impl std::fmt::Display, responded: bool) -> anyhow::Error {
    let evidence = if responded {
        "possible response; trust unconfirmed; no input was retried"
    } else {
        "no response sent"
    };
    anyhow::anyhow!("{error:#}; {evidence}")
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::super::record::{SESSION_FILE, assess_with};
    use super::super::tests::{session, setup};
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::native::session::primitives::write_json_atomic;
    #[cfg(target_os = "macos")]
    use std::fs;

    #[cfg(target_os = "macos")]
    #[test]
    fn unsupported_dialog_response_preserves_consent_without_pending_input() {
        let (temp, workspace, fixture) = setup();
        let assessment = assess_with(temp.path(), &workspace, &fixture.env).unwrap();
        let path = temp.path().join(SESSION_FILE);
        write_json_atomic(&path, &assessment).unwrap();
        let before = fs::read(&path).unwrap();
        for kind in [
            terminal::TerminalKind::WezTerm,
            terminal::TerminalKind::Warp,
            terminal::TerminalKind::Ghostty,
        ] {
            let session = terminal::TerminalSession {
                kind,
                id: "unused".into(),
                tab_id: None,
                window_id: None,
                managed_session_id: None,
                windows_process_identity: None,
                wezterm_mux: None,
            };
            let error = respond_to_workspace_dialog(
                temp.path(),
                &session,
                &terminal::GuardedDialogInput {
                    expected_claim: None,
                    expires_unix_ms: None,
                    screen: "verified dialog".into(),
                    key: terminal::DialogKey::Enter,
                },
                |_| panic!("unsupported surface must not dispatch"),
            )
            .unwrap_err();
            assert_eq!(fs::read(&path).unwrap(), before, "{kind:?}: {error:#}");
        }
    }
    #[test]
    fn complete_launch_without_a_dialog_reads_nothing() {
        complete_launch_with(
            LaunchTrust {
                answers_launch_dialog: false,
                bypass_start: None,
                applied_marker: None,
                dialog_settle: Duration::ZERO,
            },
            || panic!("authorization"),
            || panic!("manifest"),
            |_| panic!("start signal"),
            |_, _| panic!("trust or surface"),
        )
        .unwrap();
    }
    fn unused_signal(_: &Path) -> Result<Option<bool>> {
        panic!("the decision reads the signal through its seam")
    }
    // Under bypass the provider's own start report decides: no report for this launch or a
    // started provider leaves the dialog alone, and a provider that has not started yet
    // reaches the dialog with the signal, so a dialog that is there is answered (Claude
    // Code 2.1.321 on macOS shows it under bypass; 2.1.319 did not).
    #[test]
    fn complete_launch_bypass_follows_the_provider_start_signal() {
        let (_temp, workspace, _fixture) = setup();
        let (_root, directory) = session(&workspace, "launching");
        for (yolo, started, expected) in [
            (true, None, &["authorization", "manifest", "signal"][..]),
            (true, Some(true), &["authorization", "manifest", "signal"]),
            (
                true,
                Some(false),
                &["authorization", "manifest", "signal", "dialog with signal"],
            ),
            (false, None, &["authorization", "manifest", "dialog"]),
        ] {
            let order = std::cell::RefCell::new(Vec::new());
            complete_launch_with(
                LaunchTrust {
                    answers_launch_dialog: true,
                    bypass_start: Some(unused_signal),
                    applied_marker: None,
                    dialog_settle: Duration::ZERO,
                },
                || {
                    order.borrow_mut().push("authorization");
                    Ok(true)
                },
                || {
                    order.borrow_mut().push("manifest");
                    let mut manifest = Reader::open_unchecked(&directory).manifest()?;
                    manifest.yolo = yolo;
                    Ok(manifest)
                },
                |_| {
                    order.borrow_mut().push("signal");
                    Ok(started)
                },
                |_, signal| {
                    order.borrow_mut().push(if signal.is_some() {
                        "dialog with signal"
                    } else {
                        "dialog"
                    });
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                *order.borrow(),
                expected,
                "yolo {yolo}, started {started:?}"
            );
        }
    }
    #[cfg(target_os = "macos")]
    fn started_marker(directory: &Path) -> Result<Option<bool>> {
        Ok(Some(directory.join("started").exists()))
    }
    #[cfg(target_os = "macos")]
    struct Scripted<S, D> {
        screen: S,
        dispatch: D,
    }
    #[cfg(target_os = "macos")]
    impl<S: FnMut() -> Result<String>, D: FnMut() -> Result<bool>> DialogSurface for Scripted<S, D> {
        fn screen(&mut self, _: Instant) -> Result<String> {
            (self.screen)()
        }
        fn answer(&mut self, _: &terminal::GuardedDialogInput, _: Instant) -> Result<bool> {
            (self.dispatch)()
        }
    }
    // The loop under bypass: a provider that reports its start ends the wait without a
    // screen read or a key, although its store never shows trust; a dialog that is there
    // is answered once, and after the answer only the store confirms trust.
    #[cfg(target_os = "macos")]
    #[test]
    fn bypass_dialog_loop_stops_at_the_start_signal_and_answers_a_dialog_that_is_there() {
        use super::super::record::{observe, record};
        use super::super::store::native_key;
        use super::super::tests::trust_pi;
        use crate::native::clock::FakeClock;
        for dialog_shown in [false, true] {
            let (temp, workspace, stores) = setup();
            let (root, directory) = session(&workspace, "launching");
            trust_pi(&stores, &workspace);
            let assessment = assess_with(root.path(), &workspace, &stores.env).unwrap();
            assert_eq!(assessment.state, "verified");
            record(&directory, &assessment).unwrap();
            let surface: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
                "terminal": "iterm2", "session_id": "1",
                "managed_session_id": "session-trustwait"
            }))
            .unwrap();
            let mut manifest = Reader::open_unchecked(&directory).manifest().unwrap();
            manifest.provider = "claude".into();
            manifest.yolo = true;
            let key = native_key(&workspace).unwrap();
            let dialog = format!(
                "Accessing workspace:\n{key}\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\nClaude Code'll be able to read, edit, and execute files here.\nSecurity guide\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel\n"
            );
            let (reads, answers) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
            let mut clock = FakeClock::new(Instant::now());
            let deadline = clock.now() + Duration::from_secs(30);
            if !dialog_shown {
                fs::write(directory.join("started"), b"").unwrap();
            }
            LaunchDialog {
                directory: &directory,
                target: FirstPartyCli::Claude,
                session: &surface,
                expected_claim: "1-2-3",
                deadline,
                manifest,
                bypass_start: Some(started_marker),
            }
            .complete(
                &stores.env,
                &mut clock,
                &mut Scripted {
                    screen: || {
                        reads.set(reads.get() + 1);
                        Ok(dialog.clone())
                    },
                    dispatch: || {
                        answers.set(answers.get() + 1);
                        // Claude saves the approval and only then starts the session.
                        write_json_atomic(
                            &stores.claude,
                            &serde_json::json!({"projects": {&key: {"hasTrustDialogAccepted": true}}}),
                        )
                        .unwrap();
                        fs::write(directory.join("started"), b"").unwrap();
                        Ok(true)
                    },
                },
            )
            .unwrap();
            let applied = observe(&directory);
            if dialog_shown {
                assert_eq!((reads.get(), answers.get()), (1, 1));
                assert_eq!(applied.applied_marker(), Some("provider-workspace-dialog"));
            } else {
                assert_eq!((reads.get(), answers.get()), (0, 0));
                assert_eq!(applied.applied_marker(), None);
            }
            drop(temp);
        }
    }
    #[test]
    fn complete_launch_propagates_authorization_errors_before_bypass() {
        let error = complete_launch_with(
            provider::launch_trust(FirstPartyCli::Claude),
            || bail!("authorization error"),
            || panic!("manifest"),
            |_| panic!("start signal"),
            |_, _| panic!("trust or surface"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "authorization error");
    }
    #[test]
    fn complete_launch_agy_yolo_still_checks_the_dialog() {
        let (_temp, workspace, _fixture) = setup();
        let (_root, directory) = session(&workspace, "launching");
        let order = std::cell::RefCell::new(Vec::new());
        complete_launch_with(
            provider::launch_trust(FirstPartyCli::Agy),
            || {
                order.borrow_mut().push("authorization");
                Ok(true)
            },
            || {
                order.borrow_mut().push("manifest");
                let mut manifest = Reader::open_unchecked(&directory).manifest()?;
                manifest.yolo = true;
                Ok(manifest)
            },
            |_| panic!("start signal"),
            |_, signal| {
                assert!(signal.is_none());
                order.borrow_mut().push("dialog");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*order.borrow(), ["authorization", "manifest", "dialog"]);
    }
    #[cfg(windows)]
    #[test]
    fn complete_launch_windows_claude_yolo_reaches_only_an_authorized_dialog() {
        let (_temp, workspace, _fixture) = setup();
        let (_root, directory) = session(&workspace, "launching");
        for authorized in [false, true] {
            let mut dialogs = 0;
            let result = complete_launch_with(
                provider::launch_trust(FirstPartyCli::Claude),
                || Ok(authorized),
                || {
                    assert!(authorized, "unverified consent must not read the manifest");
                    let mut manifest = Reader::open_unchecked(&directory).manifest()?;
                    manifest.provider = "claude".to_owned();
                    manifest.provider_version = "2.1.302 (Claude Code)".to_owned();
                    manifest.yolo = true;
                    Ok(manifest)
                },
                |_| panic!("start signal"),
                |manifest, signal| {
                    assert!(signal.is_none());
                    assert!(manifest.yolo);
                    assert_eq!(manifest.workspace, workspace);
                    dialogs += 1;
                    bail!("guarded dialog refused")
                },
            );
            assert_eq!(dialogs, usize::from(authorized));
            if authorized {
                assert_eq!(result.unwrap_err().to_string(), "guarded dialog refused");
            } else {
                result.unwrap();
            }
        }
    }
}
