pub(super) fn close_policy(kind: TerminalKind) -> super::close_policy::ClosePolicy {
    use super::close_policy::{apple_terminal, ghostty, iterm2, warp, wezterm, windows};
    match kind {
        TerminalKind::Iterm2 => iterm2::CLOSE_POLICY,
        TerminalKind::AppleTerminal => apple_terminal::CLOSE_POLICY,
        TerminalKind::Ghostty => ghostty::CLOSE_POLICY,
        TerminalKind::Warp => warp::CLOSE_POLICY,
        TerminalKind::WezTerm => wezterm::CLOSE_POLICY,
        TerminalKind::WindowsConsole => windows::CLOSE_POLICY,
    }
}

pub(in crate::native) mod ownership;
pub(in crate::native) mod process;

use std::{
    env,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::{
    CloseOutcome, TerminalKind, TerminalSendFailure, TerminalSendResult, TerminalSession,
    select_macos_terminal,
};

const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);

pub(in crate::native) mod apple_terminal;
mod applescript;
#[cfg(test)]
pub(in crate::native) use applescript::testing as automation_testing;
pub(in crate::native) mod screen;
pub(super) use screen::{guarded_dialog_input, read_screen};
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
    let force_new_window = mode == crate::native::settings::MacosOpenMode::NewWindow;
    if kind == TerminalKind::Warp {
        return warp::open_bound_tab_with_mode(
            command,
            directory,
            deadline,
            force_new_window,
            bind,
            unbind,
        );
    }
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("terminal startup timeout leaves no room for exact surface cleanup")?;
    let mut session = match kind {
        TerminalKind::Iterm2 => iterm2::create_tab(mode, command, directory, startup_deadline),
        TerminalKind::AppleTerminal => {
            if !force_new_window {
                // Terminal.sdef exposes the tabs collection read-only. `do script in`
                // addresses an existing tab; it is not a new-tab creation command.
                // Replace this fallback if Terminal gains a native owned-tab API.
                eprintln!(
                    "Terminal.app tab-first: its native scripting interface has no new-tab creation command; opening a new owned window"
                );
            }
            apple_terminal::create_tab(command, directory, startup_deadline)
        }
        TerminalKind::Ghostty => ghostty::create_tab_with_mode(force_new_window, startup_deadline),
        TerminalKind::Warp => unreachable!(),
        TerminalKind::WezTerm => wezterm::create_surface(
            &wezterm::Installed,
            force_new_window,
            command,
            directory,
            startup_deadline,
        ),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }?;
    let start_session = session.clone();
    let cleanup_session = session.clone();
    super::bind_surface_before_start(
        &mut session,
        bind,
        || match start_session.kind {
            // The creation command waits for the atomic binding above before it
            // starts the wrapper. No shell command is typed into the new tab.
            TerminalKind::Iterm2 => Ok(()),
            // Terminal.app typed its start when it created the tab, and the start
            // waits for the same binding. Nothing is typed after the creation.
            TerminalKind::AppleTerminal => Ok(()),
            TerminalKind::Ghostty => {
                ghostty::start_session(&start_session, command, directory, startup_deadline)
            }
            TerminalKind::Warp => unreachable!(),
            // The program of the pane waits for the binding above; nothing is typed.
            TerminalKind::WezTerm => Ok(()),
            TerminalKind::WindowsConsole => unreachable!(),
        },
        || {
            cleanup_started_surface_with(
                &cleanup_session,
                || {
                    apple_terminal::apple_terminal_startup_absent(
                        directory,
                        &cleanup_session,
                        deadline,
                    )
                },
                || close_session_until(directory, &cleanup_session, deadline).map(|_| ()),
            )
        },
        unbind,
    )?;
    Ok(session)
}

fn cleanup_started_surface_with(
    session: &TerminalSession,
    absent: impl FnOnce() -> Result<bool>,
    close: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if session.kind == TerminalKind::AppleTerminal {
        if absent()? {
            return Ok(());
        }
        bail!(
            "Terminal.app startup failed with a remaining surface; no new close or signal authority was granted and its binding is retained"
        );
    }
    close()
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::send_file(session, prompt_path, deadline),
        TerminalKind::AppleTerminal => apple_terminal::send_file(session, prompt_path, deadline),
        TerminalKind::Ghostty => ghostty::send_file(session, prompt_path, deadline),
        TerminalKind::Warp => warp::send_file(session, prompt_path, deadline),
        TerminalKind::WezTerm => {
            wezterm::send_file(&wezterm::Installed, session, prompt_path, deadline)
        }
        TerminalKind::WindowsConsole => Err(TerminalSendFailure::not_sent(anyhow::anyhow!(
            "Windows Console is only available on Windows"
        ))),
    }
}

pub(super) fn verify_surface(
    directory: &Path,
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<Option<String>> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::verify_session(session, timeout).map(Some),
        TerminalKind::AppleTerminal => apple_terminal::verify_tab(session, timeout).map(Some),
        TerminalKind::Ghostty => ghostty::verify_surface(session, timeout).map(|()| None),
        TerminalKind::Warp => warp::verify_surface(directory, session, timeout).map(Some),
        TerminalKind::WezTerm => {
            wezterm::verify_session(&wezterm::Installed, session, timeout).map(Some)
        }
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
}

pub(super) fn surface_present(
    directory: &Path,
    session: &TerminalSession,
    timeout: Duration,
) -> Result<bool> {
    if session.kind == TerminalKind::Warp {
        return warp::surface_present(directory, session, timeout);
    }
    if session.kind == TerminalKind::WezTerm {
        return wezterm::surface_present(&wezterm::Installed, session, timeout);
    }
    if session.kind == TerminalKind::Ghostty {
        return ghostty::surface_present(session, timeout);
    }
    let (label, script, args) = match session.kind {
        TerminalKind::Iterm2 => ("iTerm2", iterm2::PRESENCE_SCRIPT, vec![session.id.as_str()]),
        TerminalKind::AppleTerminal => (
            "Terminal.app",
            apple_terminal::VERIFY_TAB_SCRIPT,
            vec![
                session.id.as_str(),
                session
                    .window_id
                    .as_deref()
                    .context("Terminal.app window id is missing")?,
            ],
        ),
        _ => bail!("terminal presence probe is unsupported for this host"),
    };
    let response = applescript::run_until(label, script, &args, timeout_deadline(timeout)?)?;
    match response.as_str() {
        "missing" => Ok(false),
        "present" => Ok(true),
        tty if session.kind == TerminalKind::AppleTerminal && tty == session.id => Ok(true),
        _ => bail!("unexpected terminal presence response: {response:?}"),
    }
}

// Terminal.app's managed close requires the owner's recorded app incarnation.
// Other adapters receive the directory for their own close evidence.
pub(super) fn close_owned_session(
    directory: &Path,
    session: &TerminalSession,
    deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    match session.kind {
        TerminalKind::AppleTerminal => {
            apple_terminal::close_owned_session(directory, session, deadline)
        }
        _ => close_session_until(directory, session, deadline),
    }
}

pub(super) fn close_session_until(
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::close_session_until(session, deadline),
        TerminalKind::AppleTerminal => apple_terminal::close_session_until(session, deadline),
        TerminalKind::Ghostty => ghostty::close_session_until(session, deadline),
        TerminalKind::Warp => warp::close_session_until(directory, session, deadline),
        TerminalKind::WezTerm => {
            wezterm::close_session_until(&wezterm::Installed, session, deadline)
        }
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
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

fn close_response(terminal: TerminalKind, response: &str) -> Result<CloseOutcome> {
    match response {
        "closed" => Ok(CloseOutcome::Closed),
        "missing" => Ok(CloseOutcome::Missing),
        _ => bail!(
            "unexpected {} close response: {response:?}",
            terminal.display_name()
        ),
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn prebind_cleanup_and_legacy_iterm_close_need_no_managed_id() {
        let directory = tempfile::tempdir().unwrap();
        let legacy: TerminalSession = serde_json::from_value(serde_json::json!({
            "iterm_session_id":"native-session-id"
        }))
        .unwrap();
        assert!(legacy.managed_session_id.is_none());
        let deadline = Instant::now() + Duration::from_secs(2);
        automation_testing::with_runner(
            |_| Ok("closed".into()),
            || {
                cleanup_started_surface_with(
                    &legacy,
                    || panic!("iTerm2 needs no Terminal.app absence proof"),
                    || close_session_until(directory.path(), &legacy, deadline).map(|_| ()),
                )
                .unwrap();
                assert_eq!(
                    close_owned_session(directory.path(), &legacy, deadline).unwrap(),
                    CloseOutcome::Closed
                );
            },
        );
        // Exercise the real execution boundary: its expired deadline prevents osascript.
        let error = close_session_until(directory.path(), &legacy, Instant::now()).unwrap_err();
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
        let error = close_owned_session(directory.path(), &session, Instant::now()).unwrap_err();
        assert!(format!("{error:#}").contains("expected value"), "{error:#}");
    }

    #[test]
    fn terminal_startup_missing_app_proof_retains_binding_without_adapter() {
        let mut session: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal", "session_id": "/dev/ttys999", "window_id": "1001"
        }))
        .unwrap();
        let cleanup_session = session.clone();
        let bound = std::cell::Cell::new(false);
        let calls = std::cell::Cell::new(0);
        let result = super::super::bind_surface_before_start(
            &mut session,
            |_| {
                bound.set(true);
                Ok(())
            },
            || bail!("injected startup command failure before app owner record"),
            || {
                cleanup_started_surface_with(
                    &cleanup_session,
                    || bail!("no app identity was recorded"),
                    || {
                        calls.set(calls.get() + 1);
                        Ok(())
                    },
                )
            },
            || {
                bound.set(false);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            calls.get(),
            0,
            "unproved startup rollback reached a close adapter"
        );
        assert!(bound.get(), "unproved rollback removed the durable handle");
    }

    #[test]
    fn terminal_startup_success_never_uses_failed_start_cleanup() {
        let mut session: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal", "session_id": "/dev/ttys999", "window_id": "1001"
        }))
        .unwrap();
        super::super::bind_surface_before_start(
            &mut session,
            |_| Ok(()),
            || Ok(()),
            || panic!("successful startup must not need failure cleanup"),
            || panic!("successful binding must remain"),
        )
        .unwrap();
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
