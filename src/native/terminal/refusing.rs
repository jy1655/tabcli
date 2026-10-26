use std::{path::Path, time::Instant};

use anyhow::Result;

use super::{CloseOutcome, TerminalKind, TerminalSession};
use crate::native::failure_kind::Unsupported;

#[cfg(target_os = "linux")]
const REFUSAL: &str = "visible terminal sessions are not yet supported on Linux";
#[cfg(not(target_os = "linux"))]
const REFUSAL: &str = "visible terminal sessions are not supported on this operating system";

// A real platform capability gate, so the refusal carries the unsupported hint.
fn refusal() -> anyhow::Error {
    anyhow::Error::new(Unsupported::new(REFUSAL))
}

pub(super) fn select(_preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    Err(refusal())
}

pub(super) fn open_tab(_kind: TerminalKind, _command: &str) -> Result<TerminalSession> {
    Err(refusal())
}

pub(super) fn send_file(
    _session: &TerminalSession,
    _prompt_path: &Path,
    _deadline: Instant,
) -> Result<()> {
    Err(refusal())
}

pub(super) fn close_session(_session: &TerminalSession) -> Result<CloseOutcome> {
    Err(refusal())
}

pub(super) fn close_session_until(
    _directory: &Path,
    session: &TerminalSession,
    _deadline: std::time::Instant,
) -> Result<CloseOutcome> {
    close_session(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::failure_kind::{FailureKind, classify};

    // Compiled into every test build: Windows and macOS exercise the other-OS text,
    // Linux its own; no CI target selects this module for another operating system.
    #[test]
    fn every_refusal_keeps_its_text_and_is_unsupported() {
        let session = TerminalSession {
            kind: TerminalKind::Iterm2,
            id: String::new(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
            wezterm_mux: None,
        };
        let errors = [
            select(None).unwrap_err(),
            open_tab(TerminalKind::Iterm2, "command").unwrap_err(),
            send_file(&session, Path::new("prompt"), Instant::now()).unwrap_err(),
            close_session(&session).unwrap_err(),
            close_session_until(Path::new("session"), &session, Instant::now()).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(format!("{error:#}"), REFUSAL);
            assert_eq!(format!("{error:#?}"), format!("{REFUSAL:?}"));
            assert_eq!(classify(&error), Some(FailureKind::Unsupported));
        }
    }
}
