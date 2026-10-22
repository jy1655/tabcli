use super::launch_dialog::{DialogSurface, LaunchDialog, complete_launch};
use super::record::{SESSION_FILE, assess_with, observe, record, with_record};
use super::store::{Trust, native_key};
use super::tests::{Fixture, session, setup, trust_pi};
use crate::native::clock::Clock;
use crate::native::clock::FakeClock;
use crate::native::session::{Reader, RecordStore, Store};
use crate::native::session::{SessionState, primitives::write_json_atomic, update_status};
use crate::native::{provider, terminal};
use agent_bridge::FirstPartyCli;
use anyhow::{Result, bail};
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct ScriptedSurface<S, D> {
    screen: S,
    dispatch: D,
}

impl<S, D> DialogSurface for ScriptedSurface<S, D>
where
    S: FnMut() -> Result<String>,
    D: FnMut(&terminal::GuardedDialogInput) -> Result<bool>,
{
    fn screen(&mut self, _deadline: Instant) -> Result<String> {
        (self.screen)()
    }

    fn answer(&mut self, input: &terminal::GuardedDialogInput, _deadline: Instant) -> Result<bool> {
        (self.dispatch)(input)
    }
}

struct Harness {
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    directory: PathBuf,
    workspace: PathBuf,
    stores: Fixture,
    surface: terminal::TerminalSession,
}

impl Harness {
    fn new() -> Self {
        let (home, workspace, stores) = setup();
        let (root, directory) = session(&workspace, "launching");
        trust_pi(&stores, &workspace);
        let assessment = assess_with(root.path(), &workspace, &stores.env).unwrap();
        assert_eq!(assessment.state, "verified");
        record(&directory, &assessment).unwrap();
        let surface = serde_json::from_value(serde_json::json!({
            "terminal": "windows-console", "session_id": "1",
            "managed_session_id": "session-trustwait"
        }))
        .unwrap();
        Self {
            _home: home,
            _root: root,
            directory,
            workspace,
            stores,
            surface,
        }
    }

    fn run(
        &self,
        target: FirstPartyCli,
        clock: &mut FakeClock,
        timeout: Duration,
        screen: impl FnMut() -> Result<String>,
        dispatch: impl FnMut(&terminal::GuardedDialogInput) -> Result<bool>,
    ) -> Result<()> {
        let mut manifest = Reader::open_unchecked(&self.directory).manifest()?;
        manifest.provider = target.as_str().into();
        LaunchDialog {
            directory: &self.directory,
            target,
            session: &self.surface,
            expected_claim: "1-2-3",
            deadline: clock.now() + timeout,
            manifest,
        }
        .complete(
            &self.stores.env,
            clock,
            &mut ScriptedSurface { screen, dispatch },
        )
    }

    fn screen(&self) -> String {
        format!(
            "Accessing workspace:\n{}\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\nClaude Code'll be able to read, edit, and execute files here.\nSecurity guide\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel\n",
            native_key(&self.workspace).unwrap()
        )
    }

    fn trust_claude(&self) {
        write_json_atomic(
            &self.stores.claude,
            &serde_json::json!({
                "projects": {native_key(&self.workspace).unwrap(): {"hasTrustDialogAccepted": true}}
            }),
        )
        .unwrap();
    }

    fn close(&self) {
        update_status(&self.directory, SessionState::Closed, None, None).unwrap();
    }

    fn revoke(&self) {
        with_record(self._root.path(), &self.workspace, |path, existing| {
            let mut record = existing.unwrap();
            record.revoked = true;
            RecordStore::at(path).write_json(&record)
        })
        .unwrap();
    }
}

#[test]
fn launch_dialog_settle_boundary_and_pending_intent_precede_one_response() {
    for timeout_ms in [999, 1200] {
        let h = Harness::new();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let calls = Cell::new(0);
        let result = h.run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_millis(timeout_ms),
            || Ok(h.screen()),
            |input| {
                calls.set(calls.get() + 1);
                assert_eq!(now.get() - start, Duration::from_secs(1));
                assert_eq!(
                    observe(&h.directory).value()["applied"],
                    "workspace-dialog-response-pending"
                );
                assert_eq!(input.screen, h.screen());
                assert_eq!(input.key, terminal::DialogKey::DownEnter);
                h.trust_claude();
                Ok(true)
            },
        );
        if timeout_ms == 999 {
            assert!(result.unwrap_err().to_string().contains("no response sent"));
            assert_eq!(calls.get(), 0);
            assert_eq!(clock.slept, Duration::from_millis(999));
        } else {
            result.unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(
                observe(&h.directory).value()["applied"],
                "provider-workspace-dialog"
            );
        }
    }
}

#[test]
fn launch_dialog_resets_on_full_screen_nonmatch_and_prewrite_changed() {
    for change in ["cell", "selection", "path", "nonmatch", "helper"] {
        let h = Harness::new();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let calls = Cell::new(0);
        let original_expiry = Cell::new(None);
        let result = h.run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_secs(4),
            || {
                let original = h.screen();
                let ms = (now.get() - start).as_millis();
                Ok(if ms == 600 {
                    match change {
                        "cell" => format!("different heading\n{original}"),
                        "selection" => original.replace("❯ No, exit\nYes", "No, exit\n❯ Yes"),
                        "path" => {
                            original.replace(&native_key(&h.workspace).unwrap(), "other-workspace")
                        }
                        "nonmatch" => "Bypass Permissions mode\nNo, exit\nYes, I accept".into(),
                        _ => original,
                    }
                } else {
                    original
                })
            },
            |input| {
                let expiry = input.expires_unix_ms.expect("bounded helper expiry");
                if let Some(original) = original_expiry.get() {
                    assert_eq!(
                        expiry, original,
                        "a changed screen must not renew the helper budget"
                    );
                } else {
                    original_expiry.set(Some(expiry));
                }
                let call = calls.get() + 1;
                calls.set(call);
                if change == "helper" && call == 1 {
                    assert_eq!(now.get() - start, Duration::from_secs(1));
                    return Ok(false);
                }
                let expected_ms = if change == "helper" { 2200 } else { 1800 };
                assert_eq!((now.get() - start).as_millis(), expected_ms, "{change}");
                h.trust_claude();
                Ok(true)
            },
        );
        result.unwrap();
        assert_eq!(calls.get(), if change == "helper" { 2 } else { 1 });
    }
}

#[test]
fn launch_dialog_zero_settle_keeps_agy_first_observation() {
    let h = Harness::new();
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let now = clock.shared();
    let screen = format!(
        "Accessing workspace:\n{}\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\nNo, exit\n↑/↓ Navigate · enter Confirm",
        native_key(&h.workspace).unwrap()
    );
    let calls = Cell::new(0);
    h.run(
        FirstPartyCli::Agy,
        &mut clock,
        Duration::from_secs(1),
        || Ok(screen.clone()),
        |_| {
            calls.set(calls.get() + 1);
            assert_eq!(now.get(), start);
            write_json_atomic(
                &h.stores.agy,
                &serde_json::json!({"trustedWorkspaces":[native_key(&h.workspace).unwrap()]}),
            )
            .unwrap();
            Ok(true)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 1);
}

#[test]
fn launch_dialog_ended_session_wins_over_shared_store_initially_and_after_input() {
    for after_input in [false, true] {
        let h = Harness::new();
        if !after_input {
            h.close();
            h.trust_claude();
        }
        let mut clock = FakeClock::new(Instant::now());
        let calls = Cell::new(0);
        let error = h
            .run(
                FirstPartyCli::Claude,
                &mut clock,
                Duration::from_secs(3),
                || Ok(h.screen()),
                |_| {
                    calls.set(calls.get() + 1);
                    h.close();
                    h.trust_claude();
                    Ok(true)
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("closed"), "{error}");
        assert!(
            error.contains(if after_input {
                "trust unconfirmed"
            } else {
                "no response sent"
            }),
            "{error}"
        );
        assert_eq!(calls.get(), usize::from(after_input));
        assert_ne!(
            observe(&h.directory).value()["applied"],
            "provider-workspace-dialog"
        );
    }
}

#[test]
fn launch_dialog_rechecks_close_revoke_and_deadline_after_blocking_observation() {
    for change in ["close", "revoke", "expiry"] {
        let h = Harness::new();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let calls = Cell::new(0);
        let error = h
            .run(
                FirstPartyCli::Claude,
                &mut clock,
                Duration::from_secs(2),
                || {
                    if now.get() - start >= Duration::from_secs(1) {
                        match change {
                            "close" => h.close(),
                            "revoke" => h.revoke(),
                            _ => now.set(start + Duration::from_secs(2)),
                        }
                    }
                    Ok(h.screen())
                },
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(true)
                },
            )
            .unwrap_err()
            .to_string();
        assert_eq!(calls.get(), 0, "{change}: {error}");
        assert!(error.contains("no response sent"), "{change}: {error}");
        assert!(observe(&h.directory).value()["applied"].is_null());
    }
}

#[test]
fn launch_dialog_typed_prewrite_refusal_is_distinct_from_dispatch_error() {
    for refused in [true, false] {
        let h = Harness::new();
        let mut clock = FakeClock::new(Instant::now());
        let calls = Cell::new(0);
        let error = h
            .run(
                FirstPartyCli::Claude,
                &mut clock,
                Duration::from_secs(3),
                || Ok(h.screen()),
                |_| {
                    calls.set(calls.get() + 1);
                    let cause = anyhow::anyhow!("dialog dispatch failed");
                    Err(if refused {
                        cause.context(terminal::StaleDialogClaim)
                    } else {
                        cause
                    })
                },
            )
            .unwrap_err()
            .to_string();
        assert_eq!(calls.get(), 1, "neither error may retry input");
        assert!(error.contains("dialog dispatch failed"), "{error}");
        assert_eq!(error.contains("no response sent"), refused, "{error}");
        assert_eq!(error.contains("possible response"), !refused, "{error}");
        assert_eq!(
            observe(&h.directory).value()["applied"],
            "workspace-dialog-response-pending"
        );
    }
}

#[test]
fn launch_dialog_sent_or_uncertain_input_never_retries_or_extends_deadline() {
    for helper_error in [
        None,
        Some("partial console write"),
        Some("stale claim at guarded submission"),
    ] {
        let h = Harness::new();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let calls = Cell::new(0);
        let error = h
            .run(
                FirstPartyCli::Claude,
                &mut clock,
                Duration::from_millis(1500),
                || {
                    assert_eq!(calls.get(), 0, "after input only the store may be observed");
                    Ok(h.screen())
                },
                |_| {
                    calls.set(calls.get() + 1);
                    if let Some(error) = helper_error {
                        bail!(error);
                    }
                    Ok(true)
                },
            )
            .unwrap_err()
            .to_string();
        assert_eq!(calls.get(), 1);
        assert!(error.contains("trust unconfirmed"), "{error}");
        assert!(error.contains("possible response"), "{error}");
        assert!(!error.contains("no response sent"), "{error}");
        if let Some(helper_error) = helper_error {
            assert!(error.contains(helper_error));
        } else {
            assert_eq!(now.get() - start, Duration::from_millis(1500));
        }
        assert_eq!(
            observe(&h.directory).value()["applied"],
            "workspace-dialog-response-pending"
        );
    }
}

#[test]
fn launch_dialog_confirmed_trust_survives_applied_record_failure() {
    let h = Harness::new();
    let mut clock = FakeClock::new(Instant::now());
    let calls = Cell::new(0);
    let error = h
        .run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_secs(3),
            || Ok(h.screen()),
            |_| {
                calls.set(calls.get() + 1);
                h.trust_claude();
                write_json_atomic(&h.directory.join(SESSION_FILE), &serde_json::json!({}))?;
                Ok(true)
            },
        )
        .unwrap_err();
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        provider::workspace_trust(FirstPartyCli::Claude, &h.workspace, &h.stores.env, None)
            .unwrap(),
        Trust::Trusted(_)
    ));
    let detail = format!("{error:#}");
    assert!(detail.contains("provider trust is confirmed"), "{detail}");
    assert!(
        detail.contains("recording the Bridge applied marker failed"),
        "{detail}"
    );
    assert!(detail.contains("no input was retried"), "{detail}");
    assert!(!detail.contains("trust unconfirmed"), "{detail}");
    assert!(detail.contains("missing field `schema`"), "{detail}");
    assert!(
        error.chain().any(|cause| cause.is::<serde_json::Error>()),
        "{detail}"
    );
}

#[test]
fn launch_dialog_read_error_is_not_input() {
    let h = Harness::new();
    let mut clock = FakeClock::new(Instant::now());
    let error = h
        .run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_secs(2),
            || bail!("screen unreadable"),
            |_| panic!("input after read error"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("screen unreadable"));
    assert!(error.to_string().contains("no response sent"));
    assert!(observe(&h.directory).value()["applied"].is_null());
}

#[test]
fn launch_dialog_claim_and_original_expiry_reach_real_helper() {
    const CHILD: &str = "AB_28_CONSENT_HELPER";
    const TEST: &str =
        "native::consent::launch_tests::launch_dialog_claim_and_original_expiry_reach_real_helper";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        RecordStore::at(root.path())
            .set_directory_private()
            .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("AGENT_BRIDGE_NATIVE_STATE_DIR").unwrap());
    let mut failures = Vec::new();
    for change in ["retired", "replaced", "closed", "expired"] {
        let h = Harness::new();
        let directory = root.join(format!("session-{change}"));
        fs::create_dir_all(directory.join("events")).unwrap();
        let mut manifest = Reader::open_unchecked(&h.directory).manifest().unwrap();
        manifest.id = directory.file_name().unwrap().to_str().unwrap().into();
        write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim =
            crate::native::session::turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        record(
            &directory,
            &assess_with(&root, &h.workspace, &h.stores.env).unwrap(),
        )
        .unwrap();
        let surface = serde_json::from_value(serde_json::json!({"terminal":"windows-console", "session_id":"1", "managed_session_id":manifest.id})).unwrap();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let now = clock.shared();
        let expiry_floor = crate::native::session::primitives::unix_ms() + 3000;
        let screen = h.screen();
        terminal::with_in_process_helper(
            &directory,
            &surface,
            FirstPartyCli::Claude,
            1,
            change,
            &screen,
            || {
                let error = LaunchDialog {
                    directory: &directory,
                    target: FirstPartyCli::Claude,
                    session: &surface,
                    expected_claim: claim.token(),
                    deadline: start + Duration::from_secs(3),
                    manifest,
                }
                .complete(
                    &h.stores.env,
                    &mut clock,
                    &mut ScriptedSurface {
                        screen: || Ok(screen.clone()),
                        dispatch: |input: &terminal::GuardedDialogInput| {
                            assert_eq!(input.expected_claim.as_deref(), Some(claim.token()));
                            assert_eq!(now.get() - start, Duration::from_secs(1));
                            let expiry = input.expires_unix_ms.unwrap();
                            assert!(
                                (expiry_floor
                                    ..=crate::native::session::primitives::unix_ms() + 3000)
                                    .contains(&expiry),
                                "original expiry {expiry} vs {expiry_floor}"
                            );
                            terminal::guarded_dialog_input(
                                &directory,
                                &surface,
                                input,
                                Instant::now() + Duration::from_secs(3),
                            )
                        },
                    },
                )
                .unwrap_err();
                assert!(format!("{error:#}").contains("stale"), "{error:#}");
                let detail = format!("{error:#}");
                if !detail.contains("no response sent") || detail.contains("possible response") {
                    failures.push(format!("{change}: {detail}"));
                }
            },
        );
        claim.retain();
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn launch_dialog_entry_rejects_ended_session_before_consent_short_circuit() {
    let h = Harness::new();
    h.close();
    fs::remove_file(h.directory.join(SESSION_FILE)).unwrap();
    let error = complete_launch(
        &h.directory,
        FirstPartyCli::Claude,
        &h.surface,
        "1-2-3",
        Instant::now() + Duration::from_secs(3),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("closed"), "{error}");
    assert!(error.contains("no response sent"), "{error}");
}

#[test]
fn launch_dialog_prewrite_changed_does_not_renew_original_deadline() {
    let h = Harness::new();
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let now = clock.shared();
    let calls = Cell::new(0);
    let error = h
        .run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_millis(1500),
            || Ok(h.screen()),
            |_| {
                calls.set(calls.get() + 1);
                Ok(false)
            },
        )
        .unwrap_err()
        .to_string();
    assert_eq!(calls.get(), 1);
    assert_eq!(now.get() - start, Duration::from_millis(1500));
    assert!(error.contains("no response sent"), "{error}");
}

#[test]
fn launch_dialog_settle_starts_when_first_screen_read_completes() {
    let h = Harness::new();
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let now = clock.shared();
    let error = h
        .run(
            FirstPartyCli::Claude,
            &mut clock,
            Duration::from_millis(1799),
            || {
                if now.get() == start {
                    now.set(start + Duration::from_millis(800));
                }
                Ok(h.screen())
            },
            |_| panic!("screen read time is not settle time"),
        )
        .unwrap_err()
        .to_string();
    assert_eq!(now.get() - start, Duration::from_millis(1799));
    assert!(error.contains("no response sent"), "{error}");
}
