//! The core read policy, owned by the core Record. Every read and presence check of a core
//! Record, and of an event or a receipt in its core directory, takes a regular file reached
//! without a link at its final component: a link, dangling or not, a directory or any other
//! non-regular entry is refused, never followed. A record whose text is interpreted must be
//! strict UTF-8. Each reader keeps its own missing-record semantics and error context, and a
//! reader that bounds its read keeps its bound. Adapter-private records keep the policies of
//! [`super::RecordReader`].
use super::primitives::{is_regular_file, read_regular_record, strict_text};
use super::reads::{read_event_within_budget, read_timeline_record};
use super::{RecordStore, Serialize};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// One core Record of a Session, or an entry of a core directory, read under the core read
/// policy.
#[derive(Clone, Debug)]
pub(in crate::native) struct CoreRecordReader {
    path: PathBuf,
}

/// A core Record opened for writing. Its reads are [`CoreRecordReader`]'s.
pub(in crate::native) struct CoreRecordStore {
    reader: CoreRecordReader,
}

impl std::ops::Deref for CoreRecordStore {
    type Target = CoreRecordReader;
    fn deref(&self) -> &CoreRecordReader {
        &self.reader
    }
}

impl CoreRecordReader {
    /// A core record whose address its owner already resolved, such as the Claim path a
    /// lifecycle guard holds. No I/O or validation.
    pub(in crate::native) fn at(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    pub(in crate::native) fn path(&self) -> &Path {
        &self.path
    }

    /// An entry of this core directory: an event of `events/` or a receipt of `requests/`.
    pub(in crate::native) fn child(&self, name: &str) -> Self {
        Self {
            path: self.path.join(name),
        }
    }

    /// The record's bytes; a missing record is `Ok(Err(absence))`, the platform's own
    /// report of that absence.
    fn read(&self) -> Result<std::result::Result<Vec<u8>, std::io::Error>> {
        read_regular_record(&self.path, None)
    }

    /// The record's bytes, `None` when it is missing.
    pub(in crate::native) fn bytes(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.read()?.ok())
    }

    /// The record's strict UTF-8 text, `None` when it is missing.
    pub(in crate::native) fn text(&self) -> Result<Option<String>> {
        self.bytes()?
            .map(|bytes| strict_text(&self.path, bytes))
            .transpose()
    }

    /// The record's strict UTF-8 text. A missing record is the platform's report of the
    /// absence alone, for the caller's own context.
    pub(in crate::native) fn required_text(&self) -> Result<String> {
        match self.read()? {
            Ok(bytes) => strict_text(&self.path, bytes),
            Err(absence) => Err(absence.into()),
        }
    }

    /// The record decoded by `shape` from its strict text. A missing record is
    /// `failed to read {path}` with the platform's report, a shape error `invalid JSON in
    /// {path}`.
    pub(in crate::native) fn json_with<T>(
        &self,
        shape: impl FnOnce(&str) -> serde_json::Result<T>,
    ) -> Result<T> {
        let text = match self.read()? {
            Ok(bytes) => strict_text(&self.path, bytes)?,
            Err(absence) => {
                return Err(absence)
                    .with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        shape(&text).with_context(|| format!("invalid JSON in {}", self.path.display()))
    }

    /// [`Self::json_with`] with serde's decoder of `T`, for tests that inspect a record.
    #[cfg(test)]
    pub(in crate::native) fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        self.json_with(|text| serde_json::from_str(text))
    }

    /// [`Self::json_with`] for a record that may be missing: `None` then.
    pub(in crate::native) fn optional_json_with<T>(
        &self,
        shape: impl FnOnce(&str) -> serde_json::Result<T>,
    ) -> Result<Option<T>> {
        self.text()?
            .map(|text| {
                shape(&text).with_context(|| format!("invalid JSON in {}", self.path.display()))
            })
            .transpose()
    }

    /// Whether the record is present. A missing record is `false`; a link, dangling or not,
    /// or another non-regular entry is refused as every read refuses it.
    pub(in crate::native) fn present(&self) -> Result<bool> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("refusing non-regular session file: {}", self.path.display())
            }
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", self.path.display()))
            }
        }
    }

    /// Whether a regular file is at the record's path. A link or another non-regular entry
    /// is never followed and is not the record, like a missing one: `false`.
    pub(in crate::native) fn is_regular_file(&self) -> Result<bool> {
        is_regular_file(&self.path)
    }

    /// The timeline's bounded read: strict UTF-8 within `limit` bytes, busy when the record
    /// changes under the read.
    pub(in crate::native) fn timeline(&self, limit: u64) -> Result<Option<Vec<u8>>> {
        read_timeline_record(&self.path, limit)
    }

    /// The search's budgeted read of an event's bytes; see [`read_event_within_budget`].
    pub(in crate::native) fn event_within_budget(
        &self,
        limit: u64,
    ) -> Result<Option<Vec<u8>>, Result<u64>> {
        read_event_within_budget(&self.path, limit)
    }
}

impl CoreRecordStore {
    /// A core record whose address its owner already resolved. No I/O or validation.
    pub(in crate::native) fn at(path: &Path) -> Self {
        Self {
            reader: CoreRecordReader::at(path),
        }
    }

    pub(in crate::native) fn child(&self, name: &str) -> Self {
        Self {
            reader: self.reader.child(name),
        }
    }

    fn store(&self) -> RecordStore {
        RecordStore::at(&self.path)
    }

    pub(in crate::native) fn write_json<T: Serialize>(&self, value: &T) -> Result<()> {
        self.store().write_json(value)
    }

    pub(in crate::native) fn write_private(&self, bytes: &[u8]) -> Result<()> {
        self.store().write_private(bytes)
    }

    pub(in crate::native) fn remove(&self) -> Result<()> {
        self.store().remove()
    }

    pub(in crate::native) fn remove_raw(&self) -> std::io::Result<()> {
        self.store().remove_raw()
    }

    #[cfg(all(test, windows))]
    pub(in crate::native) fn rename_to(&self, to: &CoreRecordStore) -> Result<()> {
        self.store().rename_to(&to.store())
    }
}
