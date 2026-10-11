pub(in crate::native) mod ownership;
pub(in crate::native) mod process;

use std::{
    env,
    path::Path,
    time::{Duration, Instant},
};

use crate::native::settings::MacosOpenMode;
use anyhow::{Context, Result, bail};

use super::{
    CloseOutcome, DialogKey, GuardedDialogInput, TerminalKind, TerminalSendFailure,
    TerminalSendResult, TerminalSession, select_macos_terminal,
};

const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);

pub(in crate::native) mod apple_terminal;
mod applescript;
#[cfg(test)]
pub(in crate::native) use applescript::testing as automation_testing;
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

pub(super) type BindSurface<'a> = Box<dyn FnOnce(&mut TerminalSession) -> Result<()> + 'a>;

pub(super) trait MacTerminalAdapter {
    fn open_bound_tab(
        &self,
        mode: MacosOpenMode,
        command: &str,
        directory: &Path,
        deadline: Instant,
        bind: BindSurface<'_>,
        unbind: Box<dyn FnOnce() -> Result<()> + '_>,
    ) -> Result<TerminalSession>;
    fn send_file(
        &self,
        session: &TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> TerminalSendResult;
    fn verify_surface(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Option<Duration>,
    ) -> Result<Option<String>>;
    fn surface_present(
        &self,
        directory: &Path,
        session: &TerminalSession,
        timeout: Duration,
    ) -> Result<bool>;
    fn read_screen(&self, session: &TerminalSession, deadline: Instant) -> Result<String>;
    fn guarded_dialog_input(
        &self,
        session: &TerminalSession,
        input: &GuardedDialogInput,
        deadline: Instant,
    ) -> Result<bool>;
    fn close_session_until(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome>;
    fn close_owned_session(
        &self,
        directory: &Path,
        session: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome>;
}

pub(super) fn adapter(kind: TerminalKind) -> Result<&'static dyn MacTerminalAdapter> {
    match kind {
        TerminalKind::Iterm2 => Ok(&iterm2::ADAPTER),
        TerminalKind::AppleTerminal => Ok(&apple_terminal::ADAPTER),
        TerminalKind::Ghostty => Ok(&ghostty::ADAPTER),
        TerminalKind::WezTerm => Ok(&wezterm::ADAPTER),
        TerminalKind::Warp => Ok(&warp::ADAPTER),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
}

pub(super) fn open_bound_tab<F, U>(
    kind: TerminalKind,
    command: &str,
    directory: &Path,
    deadline: Instant,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    // Read once for creation. Close uses only the persisted target/ownership record,
    // never a setting that the user may have changed since this launch.
    let mode = crate::native::settings::macos_open_mode(
        directory
            .parent()
            .context("session directory has no state root")?,
    )?;
    adapter(kind)?.open_bound_tab(
        mode,
        command,
        directory,
        deadline,
        Box::new(bind),
        Box::new(unbind),
    )
}

pub(super) fn open_with(
    deadline: Instant,
    create: impl FnOnce(Instant) -> Result<TerminalSession>,
    start: impl FnOnce(&TerminalSession, Instant) -> Result<()>,
    cleanup: impl FnOnce(&TerminalSession, Instant) -> Result<()>,
    bind: impl FnOnce(&mut TerminalSession) -> Result<()>,
    unbind: impl FnOnce() -> Result<()>,
) -> Result<TerminalSession> {
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("terminal startup timeout leaves no room for exact surface cleanup")?;
    let mut session = create(startup_deadline)?;
    let start_session = session.clone();
    let cleanup_session = session.clone();
    super::bind_surface_before_start(
        &mut session,
        bind,
        || start(&start_session, startup_deadline),
        || cleanup(&cleanup_session, deadline),
        unbind,
    )?;
    Ok(session)
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    adapter(session.kind)
        .map_err(TerminalSendFailure::not_sent)?
        .send_file(session, prompt_path, deadline)
}

pub(super) fn verify_surface(
    directory: &Path,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<Option<String>> {
    adapter(session.kind)?.verify_surface(directory, session, timeout)
}

pub(super) fn surface_present(
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    adapter(session.kind)?.surface_present(directory, session, timeout)
}

pub(super) fn read_screen(session: &TerminalSession, deadline: Instant) -> Result<String> {
    adapter(session.kind)?.read_screen(session, deadline)
}

pub(super) fn guarded_dialog_input(
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    adapter(session.kind)?.guarded_dialog_input(session, input, deadline)
}

pub(super) fn close_session_until(
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    adapter(session.kind)?.close_session_until(directory, session, deadline)
}

pub(super) fn close_owned_session(
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    adapter(session.kind)?.close_owned_session(directory, session, deadline)
}

pub(super) fn timeout_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .context("terminal automation timeout is too large")
}

// Whether the screen of the login session is locked, read from IOKit's console-user
// record (`CGSSessionScreenIsLocked` in `ioreg -n Root -d1`). None when the record could
// not be read before the deadline.
fn screen_locked(deadline: Instant) -> Option<bool> {
    let mut command = ioreg_command();
    let output = crate::native::command_output_until(&mut command, deadline, "ioreg").ok()?;
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
        let calls = std::rc::Rc::new(Cell::new(0));
        let runner_calls = calls.clone();
        let unbound = Cell::new(false);
        let adapter = adapter(TerminalKind::Iterm2).unwrap();
        automation_testing::with_runner(
            move |_| {
                let calls = &runner_calls;
                calls.set(calls.get() + 1);
                Ok(if calls.get() == 1 {
                    "native-session-id"
                } else {
                    "closed"
                }
                .into())
            },
            || {
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
                assert_eq!(calls.get(), 2, "create then exact cleanup");
                assert!(unbound.get());
                assert_eq!(
                    adapter
                        .close_owned_session(directory.path(), &legacy, deadline)
                        .unwrap(),
                    CloseOutcome::Closed
                );
            },
        );
        let error = adapter
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
        let error = adapter(session.kind)
            .unwrap()
            .close_owned_session(directory.path(), &session, Instant::now())
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
            &apple_terminal::ADAPTER.start,
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
                &apple_terminal::ADAPTER.start,
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
            kind,
            id: "surface".into(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        }
    }

    #[test]
    fn lookup_selects_each_operation_set_and_rejects_windows() {
        let deadline = Instant::now() + Duration::from_secs(2);
        for (kind, expected) in [
            (TerminalKind::Iterm2, "screen"),
            (
                TerminalKind::AppleTerminal,
                "Terminal window identity missing",
            ),
            (
                TerminalKind::Ghostty,
                "managed dialog input unsupported for this terminal",
            ),
            (
                TerminalKind::Warp,
                "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
            ),
            (
                TerminalKind::WezTerm,
                "WezTerm handle does not name the process that owns its pane",
            ),
        ] {
            let session = handle(kind);
            let actual = automation_testing::with_runner(
                |_| Ok("AB_SCREEN_BEGINscreenAB_SCREEN_END".into()),
                || adapter(kind).unwrap().read_screen(&session, deadline),
            );
            let actual = actual.unwrap_or_else(|error| error.to_string());
            assert_eq!(actual, expected, "{kind:?}");
        }
        assert_eq!(
            adapter(TerminalKind::WindowsConsole)
                .err()
                .expect("unsupported kind")
                .to_string(),
            "Windows Console is only available on Windows"
        );
    }

    #[test]
    fn unsupported_guarded_input_and_windows_send_keep_their_classification() {
        let input = GuardedDialogInput {
            screen: "screen".into(),
            key: DialogKey::Enter,
        };
        for (kind, expected) in [
            (
                TerminalKind::AppleTerminal,
                "Terminal window identity missing",
            ),
            (
                TerminalKind::Ghostty,
                "managed dialog input unsupported for this terminal",
            ),
            (
                TerminalKind::Warp,
                "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
            ),
            (
                TerminalKind::WezTerm,
                "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane",
            ),
        ] {
            assert_eq!(
                guarded_dialog_input(&handle(kind), &input, Instant::now())
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
        let failure = send_file(
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
        let failure = send_file(
            &handle(TerminalKind::Warp),
            Path::new("unused"),
            Instant::now(),
        )
        .unwrap_err();
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
        assert_eq!(
            surface_present(
                directory.path(),
                &handle(TerminalKind::AppleTerminal),
                Duration::from_secs(2)
            )
            .unwrap_err()
            .to_string(),
            "Terminal.app window id is missing"
        );
        assert_eq!(
            surface_present(
                directory.path(),
                &handle(TerminalKind::Warp),
                Duration::from_secs(2)
            )
            .unwrap_err()
            .to_string(),
            "Warp terminal handle is missing its managed session binding"
        );
        assert_eq!(
            surface_present(
                directory.path(),
                &handle(TerminalKind::WezTerm),
                Duration::from_secs(2)
            )
            .unwrap_err()
            .to_string(),
            "WezTerm handle does not name the process that owns its pane"
        );
        automation_testing::with_runner(
            |_| bail!("injected Ghostty presence failure"),
            || {
                assert_eq!(
                    surface_present(
                        directory.path(),
                        &handle(TerminalKind::Ghostty),
                        Duration::from_secs(2)
                    )
                    .unwrap_err()
                    .to_string(),
                    "injected Ghostty presence failure"
                );
            },
        );
    }

    #[test]
    fn open_with_preserves_prebind_handles_deadlines_and_failure_order() {
        use std::cell::RefCell;
        for (failure, cleanup_fails, unbind_fails, expected) in [
            ("none", false, false, vec!["create", "bind", "start"]),
            ("create", false, false, vec!["create"]),
            (
                "bind",
                false,
                false,
                vec!["create", "bind", "cleanup", "unbind"],
            ),
            (
                "start",
                false,
                false,
                vec!["create", "bind", "start", "cleanup", "unbind"],
            ),
            ("bind", true, false, vec!["create", "bind", "cleanup"]),
            (
                "start",
                true,
                false,
                vec!["create", "bind", "start", "cleanup"],
            ),
            (
                "bind",
                false,
                true,
                vec!["create", "bind", "cleanup", "unbind"],
            ),
            (
                "start",
                false,
                true,
                vec!["create", "bind", "start", "cleanup", "unbind"],
            ),
        ] {
            let trace = RefCell::new(Vec::new());
            let deadline = Instant::now() + Duration::from_secs(10);
            let startup = deadline - Duration::from_secs(2);
            let result = open_with(
                deadline,
                |received| {
                    trace.borrow_mut().push("create");
                    assert_eq!(received, startup);
                    if failure == "create" {
                        bail!("create failed");
                    }
                    Ok(handle(TerminalKind::Iterm2))
                },
                |session, received| {
                    trace.borrow_mut().push("start");
                    assert_eq!(received, startup);
                    assert!(session.managed_session_id.is_none());
                    if failure == "start" {
                        bail!("start failed");
                    }
                    Ok(())
                },
                |session, received| {
                    trace.borrow_mut().push("cleanup");
                    assert_eq!(received, deadline);
                    assert!(session.managed_session_id.is_none());
                    if cleanup_fails {
                        bail!("cleanup failed");
                    }
                    Ok(())
                },
                |session| {
                    trace.borrow_mut().push("bind");
                    session.managed_session_id = Some("bound".into());
                    if failure == "bind" {
                        bail!("bind failed");
                    }
                    Ok(())
                },
                || {
                    trace.borrow_mut().push("unbind");
                    if unbind_fails {
                        bail!("unbind failed");
                    }
                    Ok(())
                },
            );
            assert_eq!(*trace.borrow(), expected);
            if failure == "none" {
                assert_eq!(result.unwrap().managed_session_id.as_deref(), Some("bound"));
            } else {
                let error = result.unwrap_err().to_string();
                let mut expected = format!("{failure} failed");
                if cleanup_fails {
                    expected.push_str("; exact surface cleanup also failed: cleanup failed");
                }
                if unbind_fails {
                    expected.push_str("; exact surface cleanup succeeded but its durable binding could not be removed: unbind failed");
                }
                assert_eq!(error, expected);
            }
        }
        let error = open_with(
            Instant::now(),
            |_| panic!("no creation without reserve"),
            |_, _| panic!(),
            |_, _| panic!(),
            |_| panic!(),
            || panic!(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "terminal startup timeout leaves no room for exact surface cleanup"
        );
    }
}
