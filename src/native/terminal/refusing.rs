use std::{path::Path, time::Instant};

use anyhow::{Result, bail};

use super::{CloseOutcome, TerminalKind, TerminalSession};

#[cfg(target_os = "linux")]
const REFUSAL: &str = "visible terminal sessions are not yet supported on Linux";
#[cfg(not(target_os = "linux"))]
const REFUSAL: &str = "visible terminal sessions are not supported on this operating system";

pub(super) fn select(_preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    bail!(REFUSAL)
}

pub(super) fn open_tab(_kind: TerminalKind, _command: &str) -> Result<TerminalSession> {
    bail!(REFUSAL)
}

pub(super) fn send_file(
    _session: &TerminalSession,
    _prompt_path: &Path,
    _deadline: Instant,
) -> Result<()> {
    bail!(REFUSAL)
}

pub(super) fn close_session(_session: &TerminalSession) -> Result<CloseOutcome> {
    bail!(REFUSAL)
}

pub(super) fn close_session_until(
    _directory: &Path,
    session: &TerminalSession,
    _deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    close_session(session)
}
