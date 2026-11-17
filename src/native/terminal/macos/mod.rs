pub(super) use super::bound_host;
#[cfg(test)]
#[path = "bound_host.rs"]
mod bound_host_app_tests;
mod close_transaction;
pub(in crate::native::terminal) mod launch_host;
pub(in crate::native) use super::posix_ownership as ownership;
pub(in crate::native) mod process;
pub(in crate::native) mod shell_environment;

use std::{env, time::Instant};
#[cfg(test)]
use std::{path::Path, time::Duration};

use crate::native::failure_kind::Unsupported;
use crate::native::settings::MacosOpenMode;
use anyhow::{Result, bail};

use super::{
    CloseOutcome, DialogKey, GuardedDialogInput, TerminalKind, TerminalSendFailure,
    TerminalSendResult, TerminalSession, macos_selection::select_macos_terminal,
};

pub(super) use super::posix::PosixTerminalAdapter as MacTerminalAdapter;
pub(super) use super::posix::*;

pub(in crate::native) mod apple_terminal;
mod applescript;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(in crate::native) use applescript::testing as automation_testing;
#[cfg(test)]
use test_support::Unused as UnusedAdapter;
pub(in crate::native) mod ghostty;
pub(in crate::native) mod iterm2;
pub(in crate::native) mod warp;
pub(in crate::native) mod wezterm;

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    let term_program = env::var("TERM_PROGRAM").ok();
    let term = env::var("TERM").ok();
    Ok(select_macos_terminal(
        preferred,
        term_program.as_deref(),
        term.as_deref(),
        env::var_os("ITERM_SESSION_ID").is_some(),
        env::var_os("TERM_SESSION_ID").is_some(),
    ))
}

/// The five macOS adapters, each over the dependency its own constructor took.
/// Generic so that a test adapter needs neither `Sync` nor `'static`: the recording
/// automation is reference counted, the WezTerm test host is deliberately not `Sync`,
/// and Terminal.app's observation constructor borrows local callbacks.
pub(super) struct Dispatch<I, T, G, Z, W> {
    iterm2: I,
    apple_terminal: T,
    ghostty: G,
    wezterm: Z,
    warp: W,
}

impl<I, T, G, Z, W> Dispatch<I, T, G, Z, W> {
    pub(super) const fn new(iterm2: I, apple_terminal: T, ghostty: G, wezterm: Z, warp: W) -> Self {
        Self {
            iterm2,
            apple_terminal,
            ghostty,
            wezterm,
            warp,
        }
    }
}

impl<I, T, G, Z, W> Adapters for Dispatch<I, T, G, Z, W>
where
    I: MacTerminalAdapter,
    T: MacTerminalAdapter,
    G: MacTerminalAdapter,
    Z: MacTerminalAdapter,
    W: MacTerminalAdapter,
{
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn MacTerminalAdapter> {
        match kind {
            TerminalKind::Iterm2 => Ok(&self.iterm2),
            TerminalKind::AppleTerminal => Ok(&self.apple_terminal),
            TerminalKind::Ghostty => Ok(&self.ghostty),
            TerminalKind::WezTerm => Ok(&self.wezterm),
            TerminalKind::Warp => Ok(&self.warp),
            TerminalKind::WindowsConsole => bail!(Unsupported::new(
                "Windows Console is only available on Windows"
            )),
            TerminalKind::Tmux => bail!(Unsupported::new("tmux is only available on Linux")),
        }
    }
}

type InstalledDispatch = Dispatch<
    iterm2::InstalledAdapter,
    apple_terminal::InstalledAdapter,
    ghostty::InstalledAdapter,
    wezterm::InstalledAdapter,
    warp::InstalledAdapter,
>;

static INSTALLED_DISPATCH: InstalledDispatch = Dispatch::new(
    iterm2::Adapter::installed(),
    apple_terminal::Adapter::installed(),
    ghostty::Adapter::installed(),
    wezterm::Adapter::installed(),
    warp::Adapter::installed(),
);

/// The adapters over the installed terminals: `osascript` for iTerm2 and Terminal.app,
/// Ghostty's scripting dictionary and `osascript`, the installed `wezterm`, and the
/// installed Warp Control clients. Production passes this to every shared operation.
pub(super) static INSTALLED: &(dyn Adapters + Sync) = &INSTALLED_DISPATCH;

/// A dispatch for a test of Terminal.app alone, over the given automation; the other
/// kinds' adapters fail the test when reached.
#[cfg(test)]
pub(super) fn apple_terminal_dispatch(automation: automation_testing::Replay) -> impl Adapters {
    Dispatch::new(
        UnusedAdapter("iTerm2"),
        apple_terminal::with_automation(automation),
        UnusedAdapter("Ghostty"),
        UnusedAdapter("WezTerm"),
        UnusedAdapter("Warp"),
    )
}

// Whether the screen of the login session is locked, read from IOKit's console-user
// record (`CGSSessionScreenIsLocked` in `ioreg -n Root -d1`). None when the record could
// not be read before the deadline.
fn screen_locked(deadline: Instant) -> Option<bool> {
    let mut command = ioreg_command();
    let output =
        crate::native::process_env::command_output_until(&mut command, deadline, "ioreg").ok()?;
    output
        .status
        .success()
        .then(|| screen_locked_in(&String::from_utf8_lossy(&output.stdout)))
}

fn ioreg_command() -> std::process::Command {
    let mut command = crate::native::process_env::helper_command("/usr/sbin/ioreg");
    command.args(["-n", "Root", "-d", "1"]);
    command
}

fn screen_locked_in(ioreg: &str) -> bool {
    ioreg.contains("\"CGSSessionScreenIsLocked\"=Yes")
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn prebind_cleanup_and_legacy_iterm_close_need_no_managed_id() {
        let directory = tempfile::tempdir().unwrap();
        let legacy: TerminalSession = serde_json::from_value(serde_json::json!({
            "iterm_session_id":"native-session-id"
        }))
        .unwrap();
        assert!(legacy.managed_session_id.is_none());
        let deadline = Instant::now() + Duration::from_secs(10);
        let unbound = Cell::new(false);
        // Creation, the exact cleanup of the failed binding, then the legacy close.
        let replay = automation_testing::Replay::normal(&["native-session-id", "closed", "closed"]);
        let adapter = iterm2::with_automation(replay.clone());
        let error = adapter
            .open_bound_tab(
                MacosOpenMode::TabFirst,
                "true",
                directory.path(),
                deadline,
                Box::new(|session| {
                    assert!(session.managed_session_id.is_none());
                    assert_eq!(session.id, legacy.id);
                    bail!("injected binding failure")
                }),
                Box::new(|| {
                    unbound.set(true);
                    Ok(())
                }),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "injected binding failure");
        assert_eq!(replay.calls().len(), 2, "create then exact cleanup");
        assert!(unbound.get());
        assert_eq!(
            adapter
                .close_owned_session(
                    directory.path(),
                    "unused",
                    &legacy,
                    OwnedSurfaceClose::SurfaceOnly,
                    deadline
                )
                .unwrap(),
            CloseOutcome::Closed
        );
        replay.assert_finished();
        // Expiry is the installed automation's own check, made before anything runs.
        let error = iterm2::Adapter::installed()
            .close_session_until(directory.path(), &legacy, Instant::now())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "iTerm2 automation timed out before it started"
        );
    }

    #[test]
    fn expired_terminal_owned_close_still_reads_owner_evidence_first() {
        let directory = tempfile::tempdir().unwrap();
        let session: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal", "session_id": "/dev/ttys999", "window_id": "1001"
        }))
        .unwrap();
        std::fs::write(directory.path().join("native-session.json"), "broken").unwrap();
        let error = INSTALLED
            .adapter(session.kind)
            .unwrap()
            .close_owned_session(
                directory.path(),
                "unused",
                &session,
                OwnedSurfaceClose::SurfaceOnly,
                Instant::now(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("expected value"), "{error:#}");
    }

    #[test]
    fn terminal_startup_missing_app_proof_retains_binding_without_adapter() {
        let directory = tempfile::tempdir().unwrap();
        let bound = Cell::new(false);
        let calls = std::rc::Rc::new(Cell::new(0));
        let replay = automation_testing::Replay::normal(&["/dev/ttys999\n1001"]);
        let absent_calls = Cell::new(0);
        let start = |_: &TerminalSession, _: Instant| {
            bail!("injected startup command failure before app owner record")
        };
        let absent = |_: &Path, _: &TerminalSession, _: Instant| {
            absent_calls.set(absent_calls.get() + 1);
            bail!("no app identity was recorded")
        };
        let local = apple_terminal::with_observations(&start, &absent, replay.clone());
        let adapter: &dyn MacTerminalAdapter = &local;
        let result = adapter.open_bound_tab(
            MacosOpenMode::NewWindow,
            "true",
            directory.path(),
            Instant::now() + Duration::from_secs(10),
            Box::new(|_| {
                bound.set(true);
                Ok(())
            }),
            Box::new(|| {
                bound.set(false);
                Ok(())
            }),
        );

        calls.set(replay.calls().len());
        assert_eq!(
            calls.get(),
            1,
            "unproved startup rollback reached a close adapter"
        );
        replay.assert_finished();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("injected startup command failure before app owner record"));
        assert!(error.contains("no app identity was recorded"));
        assert_eq!(absent_calls.get(), 1);
        assert_eq!(
            calls.get() - 1,
            0,
            "unproved startup rollback reached a close adapter"
        );
        assert!(bound.get(), "unproved rollback removed the durable handle");
    }

    #[test]
    fn terminal_startup_success_never_uses_failed_start_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let calls = std::rc::Rc::new(Cell::new(0));
        let replay = automation_testing::Replay::normal(&["/dev/ttys999\n1001"]);
        let absent = |_: &Path, _: &TerminalSession, _: Instant| -> Result<bool> {
            panic!("successful startup must not need failure cleanup")
        };
        // Keep the installed adapter's real no-op start, borrowing only cleanup observation.
        let local = apple_terminal::with_observations(
            &apple_terminal::started_at_creation,
            &absent,
            replay.clone(),
        );
        let adapter: &dyn MacTerminalAdapter = &local;
        let session = adapter
            .open_bound_tab(
                MacosOpenMode::NewWindow,
                "true",
                directory.path(),
                Instant::now() + Duration::from_secs(10),
                Box::new(|session| {
                    session.managed_session_id = Some("bound".into());
                    Ok(())
                }),
                Box::new(|| panic!("successful binding must remain")),
            )
            .unwrap();
        calls.set(replay.calls().len());
        assert_eq!(calls.get(), 1, "successful startup must not close");
        replay.assert_finished();
        assert_eq!(session.managed_session_id.as_deref(), Some("bound"));
    }
}

#[cfg(test)]
mod screen_lock_tests {
    #[test]
    fn ioreg_drops_caller_session_environment() {
        crate::native::process_env::tests::assert_helper_environment(&super::ioreg_command(), &[]);
    }

    use super::screen_locked_in;

    #[test]
    fn the_lock_is_read_from_the_console_user_record() {
        let locked = r#"      "IOConsoleUsers" = ({"kCGSSessionOnConsoleKey"=Yes,"CGSSessionScreenLockedTime"=3939134436,"CGSSessionScreenIsLocked"=Yes,"kCGSSessionUserIDKey"=501})"#;
        let unlocked = r#"      "IOConsoleUsers" = ({"kCGSSessionOnConsoleKey"=Yes,"kCGSSessionUserIDKey"=501})"#;
        assert!(screen_locked_in(locked));
        assert!(!screen_locked_in(unlocked));
        assert!(!screen_locked_in(""));
    }
}

#[cfg(test)]
mod adapter_tests {
    fn with_replay(
        kind: TerminalKind,
        replies: &[&str],
        run: impl FnOnce(&dyn MacTerminalAdapter),
    ) {
        let replay = automation_testing::Replay::normal(replies);
        let absent = |_: &Path, _: &TerminalSession, _: Instant| -> Result<bool> {
            panic!("operation test must not clean up startup")
        };
        match kind {
            TerminalKind::Iterm2 => run(&iterm2::with_automation(replay.clone())),
            TerminalKind::AppleTerminal => run(&apple_terminal::with_observations(
                &apple_terminal::started_at_creation,
                &absent,
                replay.clone(),
            )),
            _ => panic!("unexpected replay adapter"),
        }
        replay.assert_finished();
    }

    use super::*;

    fn handle(kind: TerminalKind) -> TerminalSession {
        TerminalSession {
            linux_host: None,
            tmux_socket: None,
            kind,
            id: "surface".into(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        }
    }

    // Capability against behaviour: an operation a kind declares unsupported is
    // answered by the shared dispatch with the declared message and never reaches
    // the adapter, whose own answer would be the invariant failure. The expected
    // literals are independent of the registrations.
    #[test]
    fn declared_refusals_answer_before_adapters_and_windows_is_rejected() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let input = GuardedDialogInput {
            expected_claim: None,
            expires_unix_ms: None,
            screen: "screen".into(),
            key: DialogKey::Enter,
        };
        // Only iTerm2's read reaches an automation; Terminal.app's and WezTerm's
        // refuse the malformed handle first, and the other kinds never reach their
        // adapter's dependency.
        let iterm2 = automation_testing::Replay::normal(&["AB_SCREEN_BEGINscreenAB_SCREEN_END"]);
        let apple_terminal = automation_testing::Replay::normal(&[]);
        let dispatch = Dispatch::new(
            iterm2::with_automation(iterm2.clone()),
            apple_terminal::with_automation(apple_terminal.clone()),
            ghostty::Adapter::installed(),
            wezterm::Adapter::installed(),
            warp::Adapter::installed(),
        );
        let read = |kind: TerminalKind| {
            read_screen(&dispatch, &handle(kind), deadline)
                .unwrap_or_else(|error| error.to_string())
        };
        let guard = |kind: TerminalKind| {
            guarded_dialog_input(&dispatch, &handle(kind), &input, Instant::now())
                .unwrap_err()
                .to_string()
        };
        // Supported on both: the adapter answers (iTerm2 through the automation,
        // Terminal.app from its missing window identity).
        assert_eq!(read(TerminalKind::Iterm2), "screen");
        assert_eq!(
            read(TerminalKind::AppleTerminal),
            "Terminal window identity missing"
        );
        assert_eq!(
            guard(TerminalKind::AppleTerminal),
            "Terminal window identity missing"
        );
        // Screen reads supported, guarded input declared unsupported: the read reaches
        // the adapter (which refuses this malformed handle itself); the guard does not.
        assert_eq!(
            read(TerminalKind::WezTerm),
            "WezTerm handle does not name the process that owns its pane"
        );
        assert_eq!(
            guard(TerminalKind::WezTerm),
            "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane"
        );
        // Both declared unsupported, each operation with its own message.
        for (kind, expected_read, expected_guard) in [
            (
                TerminalKind::Ghostty,
                "Ghostty's scripting dictionary exposes no screen text",
                "managed dialog input unsupported for this terminal",
            ),
            (
                TerminalKind::Warp,
                "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
                "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
            ),
        ] {
            assert_eq!(read(kind), expected_read, "{kind:?}");
            assert_eq!(guard(kind), expected_guard, "{kind:?}");
        }
        // The adapters of the refusing kinds give only the invariant failure, so the
        // declared messages above can only have come from the shared dispatch.
        for (kind, name) in [
            (TerminalKind::Ghostty, "Ghostty"),
            (TerminalKind::Warp, "Warp"),
            (TerminalKind::WezTerm, "WezTerm"),
        ] {
            let adapter = dispatch.adapter(kind).unwrap();
            assert_eq!(
                adapter
                    .guarded_dialog_input(&handle(kind), &input, deadline)
                    .unwrap_err()
                    .to_string(),
                format!(
                    "{name} declares managed dialog input unsupported; the shared dispatch must refuse before its adapter"
                )
            );
            if kind != TerminalKind::WezTerm {
                assert_eq!(
                    adapter
                        .read_screen(&handle(kind), deadline)
                        .unwrap_err()
                        .to_string(),
                    format!(
                        "{name} declares managed screen reads unsupported; the shared dispatch must refuse before its adapter"
                    )
                );
            }
        }
        // Windows Console has no adapter on macOS.
        assert_eq!(
            dispatch
                .adapter(TerminalKind::WindowsConsole)
                .err()
                .expect("unsupported kind")
                .to_string(),
            "Windows Console is only available on Windows"
        );
        assert_eq!(
            read(TerminalKind::WindowsConsole),
            "Windows Console is only available on Windows"
        );
        let failure = send_file(
            &dispatch,
            &handle(TerminalKind::WindowsConsole),
            Path::new("unused"),
            Instant::now(),
        )
        .unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert_eq!(
            failure.error().to_string(),
            "Windows Console is only available on Windows"
        );
        // A real platform gate: the error carries the unsupported hint.
        assert_eq!(
            crate::native::failure_kind::classify(failure.error()),
            Some(crate::native::failure_kind::FailureKind::Unsupported)
        );
        // Warp's send refusal is the adapter's own and separate from its declared ones.
        let failure = send_file(
            &dispatch,
            &handle(TerminalKind::Warp),
            Path::new("unused"),
            Instant::now(),
        )
        .unwrap_err();
        iterm2.assert_finished();
        apple_terminal.assert_finished();
        assert!(!failure.delivery_may_have_occurred());
        assert_eq!(
            failure.error().to_string(),
            "Warp Control input.insert/input.replace only stage text and cannot submit it; managed Warp terminal input is unsupported"
        );
    }

    #[test]
    fn applescript_operations_preserve_responses_and_presence_order() {
        let directory = tempfile::tempdir().unwrap();
        for kind in [TerminalKind::Iterm2, TerminalKind::AppleTerminal] {
            let mut session = handle(kind);
            session.window_id = Some("window".into());
            let deadline = Instant::now() + Duration::from_secs(2);
            for (response, expected) in [("missing", false), ("present", true)] {
                with_replay(kind, &[response], |adapter| {
                    assert_eq!(
                        adapter
                            .surface_present(directory.path(), &session, Duration::from_secs(2))
                            .unwrap(),
                        expected
                    );
                });
            }
            with_replay(kind, &["surface"], |adapter| {
                let result =
                    adapter.surface_present(directory.path(), &session, Duration::from_secs(2));
                if kind == TerminalKind::AppleTerminal {
                    assert!(result.unwrap());
                } else {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "unexpected terminal presence response: \"surface\""
                    );
                }
            });
            with_replay(kind, &["bad", "bad", "bad"], |adapter| {
                assert_eq!(
                    adapter
                        .read_screen(&session, deadline)
                        .unwrap_err()
                        .to_string(),
                    "invalid managed screen response"
                );
                assert_eq!(
                    adapter
                        .guarded_dialog_input(
                            &session,
                            &GuardedDialogInput {
                                expected_claim: None,
                                expires_unix_ms: None,
                                screen: "".into(),
                                key: DialogKey::Enter
                            },
                            deadline
                        )
                        .unwrap_err()
                        .to_string(),
                    "unexpected guarded dialog input response"
                );
                assert_eq!(
                    adapter
                        .surface_present(directory.path(), &session, Duration::from_secs(2))
                        .unwrap_err()
                        .to_string(),
                    "unexpected terminal presence response: \"bad\""
                );
            });
            with_replay(
                kind,
                &["AB_SCREEN_BEGIN  screen\nAB_SCREEN_END"],
                |adapter| {
                    assert_eq!(
                        adapter.read_screen(&session, deadline).unwrap(),
                        "  screen\n"
                    );
                },
            );
            for key in [DialogKey::Enter, DialogKey::DownEnter] {
                for (response, expected) in [("sent", true), ("changed", false)] {
                    with_replay(kind, &[response], |adapter| {
                        assert_eq!(
                            adapter
                                .guarded_dialog_input(
                                    &session,
                                    &GuardedDialogInput {
                                        expected_claim: None,
                                        expires_unix_ms: None,
                                        screen: "".into(),
                                        key
                                    },
                                    deadline
                                )
                                .unwrap(),
                            expected
                        );
                    });
                }
            }
        }
        // Malformed handles are refused by the adapters before any dependency is
        // reached; Ghostty takes its snapshot first, so its injected failure answers.
        struct FailingSnapshot;
        impl ghostty::Runner for FailingSnapshot {
            fn capable(&mut self) -> Result<()> {
                panic!("presence needs no capability check")
            }
            fn run(&mut self, _: &str, _: &[&str], _: Instant) -> Result<String> {
                bail!("injected Ghostty presence failure")
            }
            fn run_send(
                &mut self,
                _: &str,
                _: &[&str],
                _: Instant,
            ) -> std::result::Result<String, TerminalSendFailure> {
                panic!("presence sends nothing")
            }
        }
        let apple_terminal = automation_testing::Replay::normal(&[]);
        let dispatch = Dispatch::new(
            UnusedAdapter("iTerm2"),
            apple_terminal::with_automation(apple_terminal.clone()),
            ghostty::with_runner(FailingSnapshot),
            wezterm::Adapter::installed(),
            warp::Adapter::installed(),
        );
        for (kind, expected) in [
            (
                TerminalKind::AppleTerminal,
                "Terminal.app window id is missing",
            ),
            (
                TerminalKind::Warp,
                "Warp terminal handle is missing its managed session binding",
            ),
            (
                TerminalKind::WezTerm,
                "WezTerm handle does not name the process that owns its pane",
            ),
            (TerminalKind::Ghostty, "injected Ghostty presence failure"),
        ] {
            assert_eq!(
                surface_present(
                    &dispatch,
                    directory.path(),
                    &handle(kind),
                    Duration::from_secs(2)
                )
                .unwrap_err()
                .to_string(),
                expected,
                "{kind:?}"
            );
        }
        apple_terminal.assert_finished();
    }
}
