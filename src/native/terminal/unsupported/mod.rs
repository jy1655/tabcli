use std::path::Path;

use anyhow::{Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession};

pub(super) fn select(_preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    bail!("visible terminal sessions are not supported on this operating system")
}

pub(super) fn open_tab(_kind: TerminalKind, _command: &str) -> Result<TerminalSession> {
    bail!("visible terminal sessions are not supported on this operating system")
}

pub(super) fn send_file(_session: &TerminalSession, _prompt_path: &Path) -> Result<()> {
    bail!("visible terminal sessions are not supported on this operating system")
}

pub(super) fn close_session(_session: &TerminalSession) -> Result<CloseOutcome> {
    bail!("visible terminal sessions are not supported on this operating system")
}
