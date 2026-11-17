//! Phase 1 deliberately installs no adapter. Phase 2 adds its kind registration and
//! returns its `PosixTerminalAdapter` from this lookup; all operations share dispatch.
pub(in crate::native) mod pidfd;
pub(in crate::native) mod process;
use super::{
    TerminalKind,
    posix::{Adapters, PosixTerminalAdapter},
};
use crate::native::failure_kind::Unsupported;
use anyhow::{Result, bail};
pub(super) struct Installed;
pub(super) static INSTALLED: Installed = Installed;
impl Adapters for Installed {
    fn adapter(&self, _kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
        bail!(Unsupported::new(
            "Linux has no registered managed visible-terminal adapter"
        ))
    }
}
pub(super) fn select(_preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    bail!(Unsupported::new(
        "Linux has no registered managed visible-terminal adapter"
    ))
}
#[cfg(test)]
mod tests;
