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
pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    let kind = preferred.unwrap_or_else(|| {
        if super::herdr_environment(
            std::env::var_os("HC_TERMINAL_ID").is_some_and(|id| !id.is_empty()),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        ) {
            TerminalKind::Herdr
        } else {
            TerminalKind::Tmux
        }
    });
    INSTALLED.adapter(kind)?;
    Ok(kind)
}
#[cfg(test)]
mod tests;
