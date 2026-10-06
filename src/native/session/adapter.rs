//! Adapter record policies whose publication semantics differ from replacement writes.
use super::*;
use serde::de::DeserializeOwned;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;

impl RecordStore {
    pub(in crate::native) fn write_new_json<T: Serialize>(&self, value: &T) -> Result<()> {
        self.write_new_bytes(&serde_json::to_vec_pretty(value)?)
    }

    pub(in crate::native) fn write_new_bytes(&self, bytes: &[u8]) -> Result<()> {
        let path = &self.path;
        let parent = path.parent().context("Warp record path has no parent")?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".agent-bridge-warp-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.as_file_mut().write_all(bytes)?;
        temporary.as_file_mut().flush()?;
        temporary.as_file().sync_all()?;
        fs::hard_link(temporary.path(), path).with_context(|| {
            format!("failed to publish exclusive Warp record {}", path.display())
        })?;
        // Once the link exists the waiting host may act on it, so a directory-sync failure
        // cannot be reported as "not published" and retried as the opposite decision.
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    }
}
impl RecordReader {
    pub(in crate::native) fn read_adapter_record<T: DeserializeOwned>(
        &self,
        label: &str,
        limit: u64,
    ) -> Result<T> {
        let path = &self.path;
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("{label} is missing: {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("refusing non-regular {label}: {}", path.display());
        }
        if metadata.len() > limit {
            bail!("{label} exceeds the bounded record limit");
        }
        let bytes = fs::read(path).with_context(|| format!("failed to read {label}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {label}"))
    }

    pub(in crate::native) fn read_bounded_file(mut file: File, limit: u64) -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut retained = Vec::new();
        file.take(limit + 1).read_to_end(&mut retained)?;
        Ok(retained)
    }
}
impl Reader {
    pub(in crate::native) fn validate_private_session_directory(&self) -> Result<()> {
        let directory = self.directory();
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(directory).with_context(|| {
            format!(
                "no such Warp host session directory: {}",
                directory.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing non-directory Warp host session path: {}",
                directory.display()
            );
        }
        // The hidden host executes the plan only from a session directory that another
        // account cannot replace or populate.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!("Warp host session directory is not private to the current user");
        }
        Ok(())
    }
}
