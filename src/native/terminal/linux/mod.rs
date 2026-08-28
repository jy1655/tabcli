use std::path::Path;

use anyhow::{Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession};

pub(super) fn select(_preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    bail!("visible terminal sessions are not yet supported on Linux")
}

pub(super) fn open_tab(_kind: TerminalKind, _command: &str) -> Result<TerminalSession> {
    bail!("visible terminal sessions are not yet supported on Linux")
}

pub(super) fn send_file(
    _session: &TerminalSession,
    _prompt_path: &Path,
    _timeout: std::time::Duration,
) -> Result<()> {
    bail!("visible terminal sessions are not yet supported on Linux")
}

pub(super) fn close_session(_session: &TerminalSession) -> Result<CloseOutcome> {
    bail!("visible terminal sessions are not yet supported on Linux")
}
