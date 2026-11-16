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
    let owner = session::owner::observe(&Reader::open_unchecked(directory))
        .decoded()?
        .with_context(|| "Windows terminal ownership requires a live native-session owner")?
        .owner;
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
// launch receipt, the root has not run its command. Only a missing owner record is absent:
// a linked, dangling or otherwise unreadable one may have been written by the root. A
// receipt that cannot be read proves nothing.
pub(in crate::native) fn windows_console_root_never_ran(directory: &Path) -> bool {
    session::owner::absent(&Reader::open_unchecked(directory))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::owner::damaged_tests::{
        Damaged, owner_path, read_error, write_damaged_owner,
    };
    use crate::native::session::test_support::lifecycle::link_directory;
    use crate::native::terminal::close_authority::{CloseObservation, Production};
    use std::fs;

    fn owner() -> NativeSessionOwner {
        NativeSessionOwner {
            pid: std::process::id(),
            managed_session_id: Some("session-a".to_owned()),
            ..NativeSessionOwner::default()
        }
    }

    // Close ends a never-started root alone only when no owner record is there at all: a
    // linked, dangling or unreadable record may have been written by the root.
    #[test]
    fn only_a_missing_owner_record_lets_the_root_count_as_never_run() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-a");
        fs::create_dir(&directory).unwrap();
        let path = owner_path(&directory);
        assert!(windows_console_root_never_ran(&directory));
        for damaged in [Damaged::Directory, Damaged::NonUtf8, Damaged::Oversized] {
            let _ = fs::remove_dir(&path);
            write_damaged_owner(&directory, &owner(), damaged);
            assert!(!windows_console_root_never_ran(&directory), "{damaged:?}");
        }
        fs::remove_file(&path).unwrap();
        let target = root.path().join("owner-target");
        fs::create_dir(&target).unwrap();
        if !link_directory(&target, &path) {
            eprintln!("skipped: this environment cannot create a directory link");
            return;
        }
        assert!(!windows_console_root_never_ran(&directory), "linked");
        fs::remove_dir(&target).unwrap();
        assert!(!windows_console_root_never_ran(&directory), "dangling");
    }

    // The surface-ownership probe and the close authority's presence check read the owner
    // under the one read policy.
    #[test]
    fn owner_readers_refuse_records_the_read_policy_refuses() {
        let session: crate::native::terminal::TerminalSession =
            serde_json::from_value(serde_json::json!({"terminal": "windows-console",
                "session_id": "1", "managed_session_id": "session-a"}))
            .unwrap();
        for damaged in [Damaged::Directory, Damaged::NonUtf8, Damaged::Oversized] {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("session-a");
            fs::create_dir(&directory).unwrap();
            let path = owner_path(&directory);
            write_damaged_owner(&directory, &owner(), damaged);
            let expected = read_error(&path, damaged);
            assert_eq!(
                format!(
                    "{:#}",
                    verified_windows_native_owner(&directory, "session-a").unwrap_err()
                ),
                expected,
                "{damaged:?}"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            assert_eq!(
                format!(
                    "{:#}",
                    Production::new(&directory, "session-a", &session, deadline)
                        .owner_present()
                        .unwrap_err()
                ),
                expected,
                "{damaged:?}"
            );
        }
    }
}
