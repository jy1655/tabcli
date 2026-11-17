//! Linux runs one adapter, tmux. Its kind registration is in `tmux/facts.rs`; every
//! operation goes through the shared POSIX dispatch.
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
static TMUX: tmux::Adapter<tmux::Installed> = tmux::Adapter::installed();
impl Adapters for Installed {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
        match kind {
            TerminalKind::Tmux => Ok(&TMUX),
            kind => bail!(Unsupported::new(format!(
                "{} is not available on Linux; use tmux",
                kind.display_name()
            ))),
        }
    }
}
pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    match preferred {
        None | Some(TerminalKind::Tmux) => Ok(TerminalKind::Tmux),
        Some(kind) => bail!(Unsupported::new(format!(
            "{} is not available on Linux; use tmux",
            kind.display_name()
        ))),
    }
}
#[cfg(test)]
mod tests;
