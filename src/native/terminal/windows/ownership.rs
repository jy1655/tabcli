use crate::native::session::{self, CoreRecord, Reader, launch};
use crate::native::terminal::ownership::{NativeSessionOwner, OwnerBinding};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

// What this probe requires of the owner record is stated over its attestation by
// `SurfaceProbe::owner_requirement` in `terminal::ownership`; its parity tests hold the two
// together.
#[cfg(windows)]
pub(in crate::native) fn verified_windows_native_owner(
    directory: &Path,
    expected_session_id: &str,
) -> Result<NativeSessionOwner> {
    let owner_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned();
    let owner_text = session::RecordReader::at(&owner_path)
        .text()?
        .with_context(|| "Windows terminal ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = session::record::owner(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    if owner.binding(expected_session_id) != OwnerBinding::Bound {
        bail!("native-session owner is not bound to this managed session")
    }
    let identity = owner
        .windows_process_identity
        .as_ref()
        .context("Windows native-session owner is missing its process identity")?;
    super::verify_process_identity(owner.pid, identity.creation_time, &identity.executable_path)
        .context("Windows native-session owner identity changed")?;
    Ok(owner)
}

// The console root runs the wrapper, and the wrapper records itself as the owner before
// it asks to spawn the provider. Without that record and without a spawn attempt in the
// launch receipt, the root has not run its command. A receipt that cannot be read proves
// nothing.
pub(in crate::native) fn windows_console_root_never_ran(directory: &Path) -> bool {
    !Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned()
        .exists()
        && match launch::read(&Reader::open_unchecked(directory)) {
            Ok(None) => true,
            Ok(Some(record)) => record.phase == launch::Phase::Pending,
            Err(_) => false,
        }
}

pub(in crate::native) fn windows_console_handle_path(
    directory: &Path,
    close_requested: bool,
) -> PathBuf {
    let closing = Reader::open_unchecked(directory)
        .record(CoreRecord::TerminalClosing)
        .path()
        .to_owned();
    if close_requested && closing.is_file() {
        closing
    } else {
        Reader::open_unchecked(directory)
            .record(CoreRecord::Terminal)
            .path()
            .to_owned()
    }
}
