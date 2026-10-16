//! One decoder per JSON core Record, the byte source a Snapshot is assembled from, and
//! the Live adapter over a session directory.
//!
//! A decoder is pure over bytes: strict UTF-8, the `*_unix_ms` unsigned-range rule, the
//! Record's shape, then the Record's own rules, one error each. It carries no path and no
//! caller context; every caller adds its own and keeps its own missing-record policy. The
//! Claim is a text token, not a decoded Record, and keeps its readers.
use super::schema::{SESSION_SCHEMA, SessionManifest};
use super::turn::{JournaledEventRead, PendingTurnCompletion, validate_pending_completion};
use super::{CoreRecord, Reader, SessionStatus, SnapshotBusy, launch, requests};
use anyhow::{Context, Error, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{fs::File, path::Path, path::PathBuf};

/// Why bytes did not decode into a Record.
#[derive(Debug)]
pub(in crate::native) enum DecodeError {
    /// The bytes are not the Record: not UTF-8, not JSON, a `*_unix_ms` value outside the
    /// JSON unsigned range, or not the Record's shape. A caller names the record here.
    Malformed(Error),
    /// A well-formed Record that breaks one of its rules; the message names the rule.
    Rule(Error),
}

impl DecodeError {
    /// Adds `context` to a malformed-bytes error; a broken rule keeps its own message.
    pub(in crate::native) fn context<C>(self, context: C) -> Error
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        match self {
            Self::Malformed(error) => error.context(context),
            Self::Rule(error) => error,
        }
    }

    /// The `invalid JSON in {path}` context of the lock-free readers.
    pub(in crate::native) fn in_path(self, path: &Path) -> Error {
        self.context(format!("invalid JSON in {}", path.display()))
    }
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Malformed(error) | DecodeError::Rule(error) => error,
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(error) | Self::Rule(error) => write!(f, "{error:#}"),
        }
    }
}

/// Strict UTF-8, the timestamp range rule, then the shape `T`. Shared by every decoder
/// and by the projections of a core Record that are not the Record itself.
pub(in crate::native) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    let text = std::str::from_utf8(bytes)
        .context("invalid UTF-8")
        .map_err(DecodeError::Malformed)?;
    let value: Value =
        serde_json::from_str(text).map_err(|error| DecodeError::Malformed(error.into()))?;
    timestamps_in_range(&value).map_err(DecodeError::Malformed)?;
    serde_json::from_str(text).map_err(|error| DecodeError::Malformed(error.into()))
}

// Recorded u128 values must fit the integer range supported by `json!`, or a renderer
// that re-serializes the record would panic.
fn timestamps_in_range(value: &Value) -> Result<()> {
    match value {
        Value::Array(values) => values.iter().try_for_each(timestamps_in_range),
        Value::Object(fields) => fields.iter().try_for_each(|(key, value)| {
            if key.ends_with("_unix_ms") && !value.is_null() && value.as_u64().is_none() {
                bail!("record timestamp is outside the JSON unsigned integer range");
            }
            timestamps_in_range(value)
        }),
        _ => Ok(()),
    }
}

/// `manifest.json`: the shape, the schema, and the id of the directory it names.
pub(in crate::native) fn manifest(
    bytes: &[u8],
    expected_id: &str,
) -> Result<SessionManifest, DecodeError> {
    let manifest: SessionManifest = decode(bytes)?;
    if manifest.schema != SESSION_SCHEMA {
        return Err(DecodeError::Rule(anyhow::anyhow!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        )));
    }
    if manifest.id != expected_id {
        return Err(DecodeError::Rule(anyhow::anyhow!(
            "session manifest id does not match its directory"
        )));
    }
    Ok(manifest)
}

/// `status.json` and `closed.json`. Legacy content stays readable: an absent
/// `generation` defaults, the diagnostics are optional, and an unknown state string is
/// kept as it was recorded.
pub(in crate::native) fn status(bytes: &[u8]) -> Result<SessionStatus, DecodeError> {
    decode(bytes)
}

/// `turn.completion.json`: the shape, then the completion's own validation.
pub(in crate::native) fn journal(bytes: &[u8]) -> Result<PendingTurnCompletion, DecodeError> {
    let pending: PendingTurnCompletion = decode(bytes)?;
    validate_pending_completion(&pending).map_err(DecodeError::Rule)?;
    Ok(pending)
}

/// `launch.json`: the shape, then the identity rule.
pub(in crate::native) fn launch(bytes: &[u8]) -> Result<launch::Record, DecodeError> {
    let record: launch::Record = decode(bytes)?;
    if record.schema != 1 || record.claim_token.is_empty() {
        return Err(DecodeError::Rule(anyhow::anyhow!(
            "invalid launch receipt identity"
        )));
    }
    Ok(record)
}

/// `requests/<claim>.json`: the shape, the receipt's validation, then the token it was
/// addressed by.
pub(in crate::native) fn receipt(
    bytes: &[u8],
    expected_token: &str,
) -> Result<requests::Receipt, DecodeError> {
    let receipt: requests::Receipt = decode(bytes)?;
    requests::validate(&receipt).map_err(DecodeError::Rule)?;
    if receipt.claim_token != expected_token {
        return Err(DecodeError::Rule(anyhow::anyhow!(
            "Bridge request receipt belongs to a different claim"
        )));
    }
    Ok(receipt)
}

/// What a source holds for one core Record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Read {
    Present(Vec<u8>),
    Absent,
    /// The source could not read the record; the text is what it retained.
    Fault(String),
}

/// What a source holds for the `requests/` directory: the claim tokens of its entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum ReceiptNames {
    Absent,
    Present(Vec<String>),
    /// The directory could not be listed to the end; `tokens` are the entries read
    /// before that.
    Fault {
        tokens: Vec<String>,
        error: String,
    },
}

/// The bytes a Snapshot is assembled from, under the source's own read policy. The
/// assembly decodes; the source decides where bytes come from, which faults it retains,
/// how it keeps the read consistent, and which lock it hands to the Snapshot.
pub(in crate::native) trait Source {
    /// The bytes of a core Record: manifest, status, claim, journal or launch record.
    /// `Err` is reserved for a failure the source does not retain as a fault, such as
    /// [`SnapshotBusy`] or the Live reads' own I/O errors.
    fn core(&mut self, record: CoreRecord) -> Result<Read>;
    /// The source's own context for a decoder error of `record`.
    fn locate(&self, record: CoreRecord, error: DecodeError) -> Error {
        let _ = record;
        error.into()
    }
    /// Runs once the state records and any publication read are done and before the
    /// receipt index: the Live adapter's test hook point.
    fn state_observed(&mut self) {}
    fn publication_read(
        &mut self,
        pending: &PendingTurnCompletion,
        limit: u64,
    ) -> Result<JournaledEventRead>;
    fn receipt_names(&mut self) -> ReceiptNames;
    fn receipt(&mut self, token: &str) -> Read;
    fn event_paths(&mut self) -> Result<Vec<PathBuf>>;
    /// The source's consistency check, then the lock the Snapshot keeps.
    fn finish(self) -> Result<Option<File>>;
}

/// The receipt index over `tokens`, each read through `read`: a receipt that decodes
/// under its token is kept, anything else is counted, sorted as every index is.
pub(in crate::native) fn receipt_index(
    tokens: &[String],
    mut read: impl FnMut(&str) -> Read,
) -> requests::Index {
    let mut index = requests::Index::default();
    for token in tokens {
        match read(token) {
            Read::Present(bytes) => match receipt(&bytes, token) {
                Ok(receipt) => index.receipts.push(receipt),
                Err(_) => index.unreadable += 1,
            },
            Read::Absent | Read::Fault(_) => index.unreadable += 1,
        }
    }
    index.receipts.sort_by(|a, b| {
        a.created_unix_ms
            .cmp(&b.created_unix_ms)
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    index
}

/// The Live adapter: the session directory under its shared lifecycle lock. The four
/// state records are read first and compared again at the end, so a record that changed
/// under the read makes the Snapshot busy; the manifest, the receipts, the events and the
/// launch record are each their own read, as they were before the seam.
pub(in crate::native) struct Live<'a> {
    reader: &'a Reader,
    lock: Option<File>,
    before: Vec<Option<Vec<u8>>>,
}

const STATE_RECORDS: [CoreRecord; 4] = [
    CoreRecord::Status,
    CoreRecord::TurnClaim,
    CoreRecord::Completion,
    CoreRecord::Launch,
];

impl<'a> Live<'a> {
    pub(in crate::native) fn begin(reader: &'a Reader) -> Result<Self> {
        let lock = reader.lock_shared()?;
        let before = STATE_RECORDS
            .map(|record| reader.record(record).bytes())
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            reader,
            lock,
            before,
        })
    }
}

impl Source for Live<'_> {
    fn core(&mut self, record: CoreRecord) -> Result<Read> {
        let present = |bytes: Option<Vec<u8>>| bytes.map_or(Read::Absent, Read::Present);
        match record {
            // The pinned state bytes, except the launch record, which is its own read.
            CoreRecord::Status | CoreRecord::TurnClaim | CoreRecord::Completion => {
                let index = STATE_RECORDS
                    .iter()
                    .position(|state| state.name() == record.name())
                    .expect("a pinned state record");
                Ok(present(self.before[index].clone()))
            }
            CoreRecord::Manifest => {
                let path = self.reader.record(record).path().to_owned();
                Ok(Read::Present(super::read_regular_bytes(&path)?))
            }
            _ => Ok(present(self.reader.record(record).bytes()?)),
        }
    }

    fn locate(&self, record: CoreRecord, error: DecodeError) -> Error {
        match record {
            CoreRecord::Manifest => error.in_path(self.reader.record(record).path()),
            CoreRecord::Launch => error.context("invalid launch receipt"),
            _ => error.into(),
        }
    }

    fn state_observed(&mut self) {
        super::snapshot::before_consistency_check(self.reader.directory());
    }

    fn publication_read(
        &mut self,
        pending: &PendingTurnCompletion,
        limit: u64,
    ) -> Result<JournaledEventRead> {
        // The predicate validates `events` before it opens anything under it, so a link
        // planted there is rejected instead of followed by this read.
        Reader::open_unchecked(self.reader.directory()).journaled_event_state_within(pending, limit)
    }

    fn receipt_names(&mut self) -> ReceiptNames {
        // A listing that fails leaves an empty index and the error, as `requests::list`
        // reported it before the seam.
        match requests::tokens(self.reader) {
            Ok(Some(tokens)) => ReceiptNames::Present(tokens),
            Ok(None) => ReceiptNames::Absent,
            Err(error) => ReceiptNames::Fault {
                tokens: Vec::new(),
                error: format!("{error:#}"),
            },
        }
    }

    fn receipt(&mut self, token: &str) -> Read {
        match requests::receipt_record(self.reader, token).bytes() {
            Ok(Some(bytes)) => Read::Present(bytes),
            Ok(None) => Read::Absent,
            Err(error) => Read::Fault(format!("{error:#}")),
        }
    }

    fn event_paths(&mut self) -> Result<Vec<PathBuf>> {
        self.reader.events()
    }

    fn finish(self) -> Result<Option<File>> {
        // Status also has its own writer lock. Check for a moving snapshot even with a
        // lifecycle reader lock, and support old records that have no lock file at all.
        for (index, record) in STATE_RECORDS.iter().enumerate() {
            if self.reader.record(*record).bytes()? != self.before[index] {
                return Err(SnapshotBusy.into());
            }
        }
        Ok(self.lock)
    }
}

#[cfg(test)]
mod tests;
