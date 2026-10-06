//! Record policies an adapter owns whose publication semantics differ from replacement
//! writes: exclusive first publication by hard link, bounded adapter reads, and a private
//! host directory check. The adapter names its records; this module only moves bytes.
use super::*;
use serde::de::DeserializeOwned;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;

impl RecordStore {
    /// Publishes a record exactly once: the bytes are linked into place and a second
    /// publication fails. `label` names the adapter in diagnostics and `temporary_prefix`
    /// is the adapter's own prefix for the temporary file beside the record.
    pub(in crate::native) fn write_new_json<T: Serialize>(
        &self,
        label: &str,
        temporary_prefix: &str,
        value: &T,
    ) -> Result<()> {
        self.write_new_bytes(label, temporary_prefix, &serde_json::to_vec_pretty(value)?)
    }

    pub(in crate::native) fn write_new_bytes(
        &self,
        label: &str,
        temporary_prefix: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let path = &self.path;
        let parent = path
            .parent()
            .with_context(|| format!("{label} record path has no parent"))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(temporary_prefix)
            .suffix(".tmp")
            .tempfile_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.as_file_mut().write_all(bytes)?;
        temporary.as_file_mut().flush()?;
        temporary.as_file().sync_all()?;
        fs::hard_link(temporary.path(), path).with_context(|| {
            format!(
                "failed to publish exclusive {label} record {}",
                path.display()
            )
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
    /// Refuses a session directory that another account could replace or populate.
    /// `label` names the directory's role in diagnostics.
    pub(in crate::native) fn validate_private_session_directory(&self, label: &str) -> Result<()> {
        let directory = self.directory();
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("no such {label} directory: {}", directory.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing non-directory {label} path: {}",
                directory.display()
            );
        }
        // The hidden host executes the plan only from a session directory that another
        // account cannot replace or populate.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!("{label} directory is not private to the current user");
        }
        Ok(())
    }
}
