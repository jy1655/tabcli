//! Consistent read-only session records and their publication evidence.
use super::schema::{EVENT_READ_LIMIT, SESSION_SCHEMA, SessionEvent, SessionManifest};
use super::turn::{
    JournaledEventRead, JournaledEventState, PendingTurnCompletion, validate_pending_completion,
};
use super::{
    CoreRecord, Reader, RecordReader, SessionState, SessionStatus, SnapshotBusy, launch, requests,
    turn,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};

pub(in crate::native) type Records = BTreeMap<String, std::result::Result<Option<Vec<u8>>, String>>;

pub(in crate::native) struct Snapshot {
    pub(in crate::native) manifest: SessionManifest,
    pub(in crate::native) status: SessionStatus,
    pub(in crate::native) receipts: Vec<requests::Receipt>,
    pub(in crate::native) unreadable_requests: usize,
    pub(in crate::native) request_index_error: Option<String>,
    pub(in crate::native) paths: Vec<PathBuf>,
    pub(in crate::native) claim: Option<String>,
    pub(in crate::native) pending: Option<PendingTurnCompletion>,
    pub(in crate::native) launch: Option<launch::Record>,
    /// The publication predicate's bounded read of the journal's event file, when a journal
    /// exists and the snapshot performed the read (see `published` and
    /// [`PublicationRead`]); a search defers it to the event's scan position and keeps
    /// `None` here.
    pub(in crate::native) pending_event: Option<JournaledEventRead>,
    pub(in crate::native) _lock: Option<File>,
}

#[cfg(test)]
type SnapshotHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs inside every snapshot attempt, after its state records and any publication
    /// read and before its consistency check, so a test can change a state record there
    /// and force the attempt to be retried.
    static BEFORE_CONSISTENCY_CHECK: std::cell::RefCell<Option<SnapshotHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `hook` for the snapshots `run` takes on this thread (see
/// [`BEFORE_CONSISTENCY_CHECK`]) and removes it afterwards.
#[cfg(test)]
pub(in crate::native) fn with_snapshot_hook<T>(
    hook: impl FnMut(&Path) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    BEFORE_CONSISTENCY_CHECK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
    let outcome = run();
    BEFORE_CONSISTENCY_CHECK.with(|cell| *cell.borrow_mut() = None);
    outcome
}

#[cfg(test)]
thread_local! {
    // Counts published event decode attempts, independently of path enumeration and
    // the completion journal's bounded publication comparison.
    static EVENT_READ_COUNT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(in crate::native) fn with_event_read_count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let previous = EVENT_READ_COUNT.with(|count| count.replace(Some(0)));
    let outcome = run();
    let reads = EVENT_READ_COUNT.with(|count| count.replace(previous).unwrap());
    (outcome, reads)
}

pub(in crate::native) fn before_consistency_check(directory: &Path) {
    #[cfg(test)]
    BEFORE_CONSISTENCY_CHECK.with(|cell| {
        if let Some(hook) = cell.borrow_mut().as_mut() {
            hook(directory);
        }
    });
    #[cfg(not(test))]
    let _ = directory;
}

/// When a snapshot decides the publication of a journaled event.
#[derive(Clone, Copy, Debug)]
pub(in crate::native) enum PublicationRead {
    /// Inside the snapshot, comparing at most this many bytes of the event with its
    /// journal. Every non-search query uses the fixed [`EVENT_READ_LIMIT`], so it
    /// publishes exactly what an uncontended read would publish, however many attempts a
    /// busy retry took.
    Within(u64),
    /// Not inside the snapshot: a search reads the journaled event only when its scan
    /// reaches the event's position in filename order, within the budget left at that
    /// moment, so the journal's presence never changes which earlier records the scan
    /// examines, and no retry of the snapshot costs a read. Until the caller performs
    /// that read, the journaled event counts as unpublished.
    Deferred,
}

impl Reader {
    pub(in crate::native) fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_within(EVENT_READ_LIMIT)
    }

    pub(in crate::native) fn snapshot_within(&self, event_limit: u64) -> Result<Snapshot> {
        self.snapshot_with(PublicationRead::Within(event_limit))
    }

    pub(in crate::native) fn snapshot_with(
        &self,
        publication: PublicationRead,
    ) -> Result<Snapshot> {
        Snapshot::read_with(self, publication)
    }
}

impl Snapshot {
    /// A consistent read deciding a journaled event's publication as `publication` says.
    fn read_with(reader: &Reader, publication: PublicationRead) -> Result<Self> {
        let directory = reader.directory();
        let lock = reader.lock_shared()?;
        let state_files = [
            CoreRecord::Status.name(),
            CoreRecord::TurnClaim.name(),
            CoreRecord::Completion.name(),
            CoreRecord::Launch.name(),
        ];
        let before = state_files
            .map(|name| reader.private(name).text())
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let status: SessionStatus = serde_json::from_str(
            before[0]
                .as_deref()
                .context("session has no status record")?,
        )?;
        let pending: Option<PendingTurnCompletion> =
            before[2].as_deref().map(serde_json::from_str).transpose()?;
        let pending_event = match (&pending, publication) {
            (Some(pending), PublicationRead::Within(event_limit)) => {
                validate_pending_completion(pending)?;
                // The predicate validates `events` before it opens anything under it, so
                // a link planted there is rejected instead of followed by this read.
                Some(
                    Reader::open_unchecked(directory)
                        .journaled_event_state_within(pending, event_limit)?,
                )
            }
            (Some(pending), PublicationRead::Deferred) => {
                validate_pending_completion(pending)?;
                None
            }
            (None, _) => None,
        };
        before_consistency_check(directory);
        let (index, request_index_error) = match requests::list(&Reader::open_unchecked(directory))
        {
            Ok(index) => (index, None),
            Err(error) => (requests::Index::default(), Some(format!("{error:#}"))),
        };
        let snapshot = Self {
            manifest: reader.manifest()?,
            status,
            receipts: index.receipts,
            unreadable_requests: index.unreadable,
            request_index_error,
            paths: reader.events()?,
            claim: before[1].as_deref().map(|text| text.trim().to_owned()),
            pending,
            launch: launch::read(reader)?,
            pending_event,
            _lock: lock,
        };
        // Status also has its own writer lock. Check for a moving snapshot even with a
        // lifecycle reader lock, and support old records that have no lock file at all.
        for (index, name) in state_files.iter().enumerate() {
            if reader.private(name).text()? != before[index] {
                return Err(SnapshotBusy.into());
            }
        }
        Ok(snapshot)
    }

    pub(in crate::native) fn latest_event_id(&self) -> Option<&str> {
        self.paths
            .iter()
            .rev()
            .filter_map(|p| p.file_name()?.to_str())
            .find(|name| self.published(name))
    }

    pub(in crate::native) fn receipt_for_event(&self, event: &str) -> Option<&requests::Receipt> {
        self.receipts
            .iter()
            .find(|receipt| receipt.event_file == event)
    }

    /// The completion journal naming `event`, when one exists.
    pub(in crate::native) fn journal_for(&self, event: &str) -> Option<&PendingTurnCompletion> {
        self.pending
            .as_ref()
            .filter(|pending| pending.event_file == event)
    }

    pub(in crate::native) fn published(&self, event: &str) -> bool {
        // A journaled event is published exactly when its file already holds the journal's
        // bytes: every lifecycle path (recovery, close, interrupted close) keeps such an
        // event, and none publishes a journaled event that was never written, differs, or
        // could not be compared within the read limit. The claim does not decide it: a
        // close releases the claim before it removes the journal, so the journal outlives
        // every interruption that could otherwise hide a committed event. A snapshot that
        // deferred the comparison has no verdict yet, and no verdict publishes nothing.
        super::turn::event_published(
            self.journal_for(event).is_some(),
            self.pending_event.as_ref(),
            self.receipt_for_event(event)
                .is_some_and(|receipt| self.is_active(receipt)),
        )
    }

    pub(in crate::native) fn event(
        &self,
        reader: &Reader,
        name: &str,
    ) -> Result<Option<SessionEvent>> {
        if !self.published(name) {
            return Ok(None);
        }
        #[cfg(test)]
        EVENT_READ_COUNT.with(|count| count.set(count.get().map(|reads| reads + 1)));
        RecordReader::at(reader.event(name).path()).optional_json()
    }

    pub(in crate::native) fn active_receipt(&self) -> Option<&requests::Receipt> {
        self.receipts.iter().find(|receipt| self.is_active(receipt))
    }

    pub(in crate::native) fn is_active(&self, receipt: &requests::Receipt) -> bool {
        self.claim.as_deref() == Some(&receipt.claim_token)
    }

    /// Read a journaled event once at the caller's scan position. The caller charges
    /// `bytes_read` and reuses `committed_text`; this does not change the snapshot.
    pub(in crate::native) fn publication_read(
        &self,
        reader: &Reader,
        event: &str,
        limit: u64,
    ) -> Result<JournaledEventRead> {
        let pending = self
            .journal_for(event)
            .context("event has no completion journal")?;
        Reader::open_unchecked(reader.directory()).journaled_event_state_within(pending, limit)
    }

    pub(in crate::native) fn from_records(records: &Records) -> Result<Self> {
        fn optional<T: for<'de> Deserialize<'de>>(
            records: &Records,
            source: &str,
        ) -> Result<Option<T>> {
            if records[source].is_err() {
                return Ok(None);
            }
            parsed(records, source)
        }
        let index = receipt_index(records);
        let manifest: SessionManifest =
            parsed(records, CoreRecord::Manifest.name())?.context("session has no manifest")?;
        if manifest.schema != SESSION_SCHEMA {
            bail!(
                "unsupported session schema {} for {}",
                manifest.schema,
                manifest.id
            );
        }
        let pending: Option<PendingTurnCompletion> =
            optional(records, CoreRecord::Completion.name())?;
        if let Some(p) = &pending {
            validate_pending_completion(p)?;
        }
        let pending_event = pending
            .as_ref()
            .map(|p| -> Result<JournaledEventRead> {
                let source = format!("events/{}", p.event_file);
                let bytes = records
                    .get(&source)
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|r| r.as_ref());
                let state = bytes
                    .map(|b| turn::journaled_event_state_of(p, b, EVENT_READ_LIMIT))
                    .transpose()?
                    .unwrap_or(JournaledEventState::Mismatched);
                Ok(JournaledEventRead {
                    state,
                    bytes_read: 0,
                    committed_text: None,
                })
            })
            .transpose()?;
        let launch: Option<launch::Record> = optional(records, CoreRecord::Launch.name())?;
        if launch
            .as_ref()
            .is_some_and(|l| l.schema != 1 || l.claim_token.is_empty())
        {
            bail!("invalid launch receipt identity");
        }
        let claim = records[CoreRecord::TurnClaim.name()]
            .as_ref()
            .ok()
            .and_then(|v| v.as_ref())
            .map(|b| std::str::from_utf8(b).map(|s| s.trim().to_owned()))
            .transpose()?;
        Ok(Snapshot {
            manifest,
            status: if records[CoreRecord::Status.name()].is_err() {
                SessionStatus {
                    state: SessionState::Unknown("unknown".to_owned()),
                    generation: 0,
                    updated_unix_ms: 0,
                    exit_code: None,
                    error: None,
                    residual_surface: None,
                }
            } else {
                parsed(records, CoreRecord::Status.name())?
                    .context("session has no status record")?
            },
            receipts: index.receipts,
            unreadable_requests: index.unreadable,
            request_index_error: records["requests/"].as_ref().err().cloned(),
            paths: records
                .keys()
                .filter(|s| s.starts_with("events/") && s.ends_with(".json"))
                .map(PathBuf::from)
                .collect(),
            claim,
            pending,
            launch,
            pending_event,
            _lock: None,
        })
    }
}

pub(in crate::native) fn parsed<T: for<'de> Deserialize<'de>>(
    records: &Records,
    source: &str,
) -> Result<Option<T>> {
    let bytes = records
        .get(source)
        .context("record not selected")?
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    bytes
        .as_ref()
        .map(|b| {
            let text = std::str::from_utf8(b).context("invalid UTF-8")?;
            // Recorded u128 values must fit the integer range supported by json!.
            fn numbers(v: &Value) -> Result<()> {
                match v {
                    Value::Array(vs) => {
                        for v in vs {
                            numbers(v)?;
                        }
                    }
                    Value::Object(vs) => {
                        for (key, v) in vs {
                            if key.ends_with("_unix_ms") && !v.is_null() && v.as_u64().is_none() {
                                bail!(
                                    "record timestamp is outside the JSON unsigned integer range"
                                );
                            }
                            numbers(v)?;
                        }
                    }
                    _ => (),
                }
                Ok(())
            }
            numbers(&serde_json::from_str::<Value>(text)?)?;
            serde_json::from_str(text).context("invalid JSON record")
        })
        .transpose()
}

pub(in crate::native) fn receipt_index(records: &Records) -> requests::Index {
    let mut index = requests::Index::default();
    for source in records
        .keys()
        .filter(|s| s.starts_with("requests/") && s.ends_with(".json"))
    {
        let read = parsed::<requests::Receipt>(records, source).and_then(|r| {
            let r = r.context("missing receipt")?;
            requests::validate(&r)?;
            if source != &format!("requests/{}.json", r.claim_token) {
                bail!("invalid Bridge request receipt");
            }
            Ok(r)
        });
        match read {
            Ok(r) => index.receipts.push(r),
            Err(_) => index.unreadable += 1,
        }
    }
    index.receipts.sort_by(|a, b| {
        a.created_unix_ms
            .cmp(&b.created_unix_ms)
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    index
}

#[cfg(test)]
mod tests;
