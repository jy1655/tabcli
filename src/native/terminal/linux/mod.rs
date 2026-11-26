//! Linux supports tmux and an already running Herdr Core. Every
//! operation goes through the shared POSIX dispatch.
pub(super) mod host;
pub(in crate::native) mod pidfd;
pub(in crate::native) mod process;
pub(in crate::native) mod tmux;
use super::{
    OsTerminals, TerminalKind,
    posix::{Adapters, PosixTerminalAdapter},
};
use anyhow::Result;
pub(super) struct Installed;
pub(super) static INSTALLED: Installed = Installed;
static HERDR: super::herdr::InstalledAdapter = super::herdr::Adapter::installed();
static TMUX: tmux::Adapter<tmux::Installed> = tmux::Adapter::installed();
impl Adapters for Installed {
    // A stored handle of a kind Linux does not run gets the refusal every OS gives such a
    // handle.
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
        match kind {
            TerminalKind::Tmux => Ok(&TMUX),
            TerminalKind::Herdr => Ok(&HERDR),
            TerminalKind::Iterm2
            | TerminalKind::AppleTerminal
            | TerminalKind::Ghostty
            | TerminalKind::Warp
            | TerminalKind::WezTerm
            | TerminalKind::WindowsConsole => Err(OsTerminals::LINUX.not_here(kind)),
        }
    }
}
// The terminal Linux chooses, Herdr's environment aside (`terminal::select_for` applies it
// the same way on every platform): an explicit kind it registers, or tmux.
pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    let kind = preferred.unwrap_or(TerminalKind::Tmux);
    // An explicit kind Linux does not run is refused naming the kinds Linux runs.
    if !OsTerminals::LINUX.runs(kind) {
        return Err(OsTerminals::LINUX.unavailable(kind));
    }
    Ok(kind)
}
#[cfg(test)]
mod tests;
