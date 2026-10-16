//! Consistent read-only session records and their publication evidence.
use super::record::{self, Live, Read, ReceiptNames, Source, Tolerance};
use super::schema::{EVENT_READ_LIMIT, SessionEvent, SessionManifest};
use super::turn::{JournaledEventRead, PendingTurnCompletion};
use super::{CoreRecord, Reader, RecordReader, SessionState, SessionStatus, launch, requests};
use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

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
type EventReadHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs on every published event decode with the event's path, so a test can observe
    /// what the reader holds while it decodes.
    static ON_EVENT_READ: std::cell::RefCell<Option<EventReadHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `hook` for the published event decodes `run` performs on this thread and
/// removes it afterwards.
#[cfg(test)]
pub(in crate::native) fn with_event_read_hook<T>(
    hook: impl FnMut(&Path) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    ON_EVENT_READ.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
    let outcome = run();
    ON_EVENT_READ.with(|cell| *cell.borrow_mut() = None);
    outcome
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
        let expected_id = self
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        Snapshot::assemble(Live::begin(self)?, expected_id, publication)
    }
}

impl Snapshot {
    /// Builds the Snapshot from `source` in the order the Live read always used: status
    /// and journal from the state bytes, the publication read as `publication` says, the
    /// receipt index, the manifest bound to `expected_id`, the events, the claim, the
    /// launch record, then the source's consistency check. A retained fault reaches the
    /// Snapshot as the source's [`Tolerance`] for that record says: it fails the read,
    /// leaves the record absent, or (status) yields the unknown status.
    pub(in crate::native) fn assemble(
        mut source: impl Source,
        expected_id: &str,
        publication: PublicationRead,
    ) -> Result<Self> {
        fn present(source: &mut impl Source, record: CoreRecord) -> Result<Option<Vec<u8>>> {
            match source.core(record)? {
                Read::Present(bytes) => Ok(Some(bytes)),
                Read::Absent => Ok(None),
                Read::Fault(error) => match source.tolerance(record) {
                    Tolerance::Absent => Ok(None),
                    Tolerance::Fail | Tolerance::UnknownStatus => bail!("{error}"),
                },
            }
        }
        let status = match source.core(CoreRecord::Status)? {
            Read::Present(bytes) => {
                record::status(&bytes).map_err(|error| source.locate(CoreRecord::Status, error))?
            }
            Read::Absent => bail!("session has no status record"),
            Read::Fault(error) => match source.tolerance(CoreRecord::Status) {
                Tolerance::UnknownStatus => SessionStatus {
                    state: SessionState::Unknown("unknown".to_owned()),
                    generation: 0,
                    updated_unix_ms: 0,
                    exit_code: None,
                    error: None,
                    residual_surface: None,
                },
                Tolerance::Fail | Tolerance::Absent => bail!("{error}"),
            },
        };
        let pending = present(&mut source, CoreRecord::Completion)?
            .map(|bytes| {
                record::journal(&bytes)
                    .map_err(|error| source.locate(CoreRecord::Completion, error))
            })
            .transpose()?;
        let pending_event = match (&pending, publication) {
            (Some(pending), PublicationRead::Within(event_limit)) => {
                Some(source.publication_read(pending, event_limit)?)
            }
            (Some(_), PublicationRead::Deferred) | (None, _) => None,
        };
        source.state_observed();
        let (index, request_index_error) = match source.receipt_names() {
            ReceiptNames::Absent => (requests::Index::default(), None),
            ReceiptNames::Present(tokens) => (
                record::receipt_index(&tokens, |token| source.receipt(token)),
                None,
            ),
            ReceiptNames::Fault { tokens, error } => (
                record::receipt_index(&tokens, |token| source.receipt(token)),
                Some(error),
            ),
        };
        let manifest = match present(&mut source, CoreRecord::Manifest)? {
            Some(bytes) => record::manifest(&bytes, expected_id)
                .map_err(|error| source.locate(CoreRecord::Manifest, error))?,
            None => bail!("session has no manifest"),
        };
        let paths = source.event_paths()?;
        // The Claim is a text token, read as the lifecycle readers read it.
        let claim = present(&mut source, CoreRecord::TurnClaim)?
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned());
        let launch = present(&mut source, CoreRecord::Launch)?
            .map(|bytes| {
                record::launch(&bytes).map_err(|error| source.locate(CoreRecord::Launch, error))
            })
            .transpose()?;
        let lock = source.finish()?;
        Ok(Self {
            manifest,
            status,
            receipts: index.receipts,
            unreadable_requests: index.unreadable,
            request_index_error,
            paths,
            claim,
            pending,
            launch,
            pending_event,
            _lock: lock,
        })
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
        let record = reader.event(name);
        #[cfg(test)]
        ON_EVENT_READ.with(|cell| {
            if let Some(hook) = cell.borrow_mut().as_mut() {
                hook(record.path());
            }
        });
        RecordReader::at(record.path()).optional_json()
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
}

#[cfg(test)]
mod tests;
