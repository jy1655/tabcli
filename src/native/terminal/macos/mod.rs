use std::{env, path::Path};

use anyhow::{Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession, select_macos_terminal};

pub(in crate::native) mod apple_terminal;
mod applescript;
pub(in crate::native) mod ghostty;
pub(in crate::native) mod iterm2;

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

pub(super) fn open_tab(kind: TerminalKind, command: &str) -> Result<TerminalSession> {
    match kind {
        TerminalKind::Iterm2 => iterm2::open_tab(command),
        TerminalKind::AppleTerminal => apple_terminal::open_tab(command),
        TerminalKind::Ghostty => ghostty::open_tab(command),
    }
}

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::send_file(session, prompt_path),
        TerminalKind::AppleTerminal => apple_terminal::send_file(session, prompt_path),
        TerminalKind::Ghostty => ghostty::send_file(session, prompt_path),
    }
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    match session.kind {
        TerminalKind::Iterm2 => iterm2::close_session(session),
        TerminalKind::AppleTerminal => apple_terminal::close_session(session),
        TerminalKind::Ghostty => ghostty::close_session(session),
    }
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
