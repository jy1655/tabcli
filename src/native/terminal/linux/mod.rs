//! Linux supports tmux and an already running Herdr Core. Every
//! operation goes through the shared POSIX dispatch.
pub(super) mod host;
pub(in crate::native) mod pidfd;
pub(in crate::native) mod process;
pub(in crate::native) mod tmux;
use super::{
    TerminalKind,
    posix::{Adapters, PosixTerminalAdapter},
};
use crate::native::failure_kind::Unsupported;
use anyhow::{Result, bail};
pub(super) struct Installed;
pub(super) static INSTALLED: Installed = Installed;
static HERDR: super::herdr::InstalledAdapter = super::herdr::Adapter::installed();
static TMUX: tmux::Adapter<tmux::Installed> = tmux::Adapter::installed();
impl Adapters for Installed {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
        match kind {
            TerminalKind::Tmux => Ok(&TMUX),
            TerminalKind::Herdr => Ok(&HERDR),
            kind => bail!(Unsupported::new(format!(
                "{} is not available on Linux; use tmux or Herdr",
                kind.display_name()
            ))),
        }
    }
}
// The terminal Linux chooses, Herdr's environment aside (`terminal::select_for` applies it
// the same way on every platform): an explicit kind it registers, or tmux.
pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    let kind = preferred.unwrap_or(TerminalKind::Tmux);
    INSTALLED.adapter(kind)?;
    Ok(kind)
}
#[cfg(test)]
mod tests;
