use crate::native::session::{self, CoreRecord, Reader};
use crate::native::terminal;
use crate::native::terminal::ownership::NativeSessionOwner;
use anyhow::{Context, Result, bail};
use std::path::Path;

#[cfg(windows)]
pub(in crate::native) fn verified_windows_native_owner(
    directory: &Path,
    expected_session_id: &str,
) -> Result<()> {
    let owner_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned();
    let owner_text = session::RecordReader::at(&owner_path)
        .text()?
        .with_context(|| "Windows terminal ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    let identity = owner
        .windows_process_identity
        .as_ref()
        .context("Windows native-session owner is missing its process identity")?;
    terminal::verify_windows_process_identity(owner.pid, identity)
        .context("Windows native-session owner identity changed")
}
