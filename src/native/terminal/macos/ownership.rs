use super::process::{live_native_process_identity, terminal_tty_device};
use crate::native::session::{self, CoreRecord, Reader};
use crate::native::terminal;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, verify_native_terminal_owner_with,
};
use anyhow::{Context, Result};
use std::path::Path;

#[cfg(target_os = "macos")]
pub(in crate::native) fn verified_macos_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let owner_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned();
    let owner_text = session::RecordReader::at(&owner_path)
        .text()?
        .with_context(|| "Terminal.app ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    verify_native_terminal_owner_with(
        expected_session_id,
        session,
        surface_tty,
        owner,
        live_native_process_identity,
        terminal_tty_device,
    )
}
