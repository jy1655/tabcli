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
mod screen;
pub(super) use screen::{guarded_dialog_input, read_screen};
pub(in crate::native) mod ghostty;
pub(in crate::native) mod iterm2;
pub(in crate::native) mod warp;

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
    if kind == TerminalKind::Warp {
        return warp::open_bound_tab(command, directory, deadline, bind, unbind);
    }
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("terminal startup timeout leaves no room for exact surface cleanup")?;
    let mut session = match kind {
        TerminalKind::Iterm2 => iterm2::create_tab(startup_deadline),
        TerminalKind::AppleTerminal => apple_terminal::create_tab(startup_deadline),
        TerminalKind::Ghostty => ghostty::create_tab(startup_deadline),
        TerminalKind::Warp => unreachable!(),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }?;
    let start_session = session.clone();
    let cleanup_session = session.clone();
    super::bind_surface_before_start(
        &mut session,
        bind,
        || match start_session.kind {
            TerminalKind::Iterm2 => {
                iterm2::start_session(&start_session, command, startup_deadline)
            }
            TerminalKind::AppleTerminal => {
                apple_terminal::start_session(&start_session, command, startup_deadline)
            }
            TerminalKind::Ghostty => {
                ghostty::start_session(&start_session, command, startup_deadline)
            }
            TerminalKind::Warp => unreachable!(),
            TerminalKind::WindowsConsole => unreachable!(),
        },
        || close_session_until(&cleanup_session, deadline).map(|_| ()),
        unbind,
    )?;
    Ok(session)
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
        TerminalKind::WindowsConsole => Err(TerminalSendFailure::not_sent(anyhow::anyhow!(
            "Windows Console is only available on Windows"
        ))),
    }
}

pub(super) fn verify_surface(
    session: &TerminalSession,
    timeout: Option<Duration>,
) -> Result<Option<String>> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::verify_session(session, timeout).map(Some),
        TerminalKind::AppleTerminal => apple_terminal::verify_tab(session, timeout).map(Some),
        TerminalKind::Ghostty => ghostty::verify_surface(session, timeout).map(|()| None),
        TerminalKind::Warp => warp::verify_surface(session, timeout).map(Some),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
}

pub(super) fn surface_present(session: &TerminalSession, timeout: Duration) -> Result<bool> {
    if session.kind == TerminalKind::Warp {
        return warp::surface_present(session, timeout);
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

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::close_session(session),
        TerminalKind::AppleTerminal => apple_terminal::close_session(session),
        TerminalKind::Ghostty => ghostty::close_session(session),
        TerminalKind::Warp => warp::close_session(session),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
}

fn close_session_until(session: &TerminalSession, deadline: Instant) -> Result<CloseOutcome> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::close_session_until(session, deadline),
        TerminalKind::AppleTerminal => apple_terminal::close_session_until(session, deadline),
        TerminalKind::Ghostty => ghostty::close_session_until(session, deadline),
        TerminalKind::Warp => warp::close_session_until(session, deadline),
        TerminalKind::WindowsConsole => bail!("Windows Console is only available on Windows"),
    }
}

pub(super) fn timeout_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .context("terminal automation timeout is too large")
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
