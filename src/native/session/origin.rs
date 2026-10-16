//! Origin observation: the managed Session recorded as the Request source of a Session's
//! Initial prompt, and whether that Origin's own records show it ended.
//!
//! Read-only. Every record is read through `RecordReader` as a regular file, strictly as
//! UTF-8 and within a `ReadBudget`; nothing here opens a `Store`, takes a lifecycle lock,
//! repairs, recovers, closes or writes. The answer describes the records at the time of
//! the read and is never a close authority.
use super::enumerate::{CheckOrder, Fault, enumerate};
use super::launch;
use super::primitives::{EXTERNAL_SOURCE, valid_session_id};
use super::record::{self, DecodeError};
use super::requests::{self, Receipt};
use super::snapshot::Snapshot;
use super::turn::valid_turn_claim_token;
use super::{CoreRecord, Reader, RecordReader, SessionState, SessionStatus, SnapshotBusy};
use crate::native::clock::{Clock, SystemClock};
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use crate::native::terminal::ownership::NativeSessionOwner;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// Largest record any Origin read accepts.
const RECORD_LIMIT: u64 = 64 * 1024;
/// Cooperative time and byte budget of one Origin observation inside a query.
const OBSERVATION_TIME: Duration = Duration::from_millis(100);
const OBSERVATION_BYTES: u64 = 512 * 1024;
/// Bounds of the started-from scan that follows a successful close.
pub(in crate::native) const SCAN_TIME: Duration = Duration::from_secs(2);
const SCAN_BYTES: u64 = 16 * 1024 * 1024;
const SCAN_ENTRIES: u64 = 4096;
const SCAN_OPEN: usize = 256;
const SCAN_REASONS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(in crate::native) enum SourceKind {
    Managed,
    External,
    Unknown,
}

/// The Origin as the initial Request records it. `id` is set only for `Managed`.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::native) struct OriginReference {
    pub(in crate::native) id: Option<String>,
    pub(in crate::native) kind: SourceKind,
    pub(in crate::native) request_id: Option<String>,
    pub(in crate::native) reason: String,
    pub(in crate::native) detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::native) struct OriginObservation {
    pub(in crate::native) reference: OriginReference,
    pub(in crate::native) stored_state: Option<String>,
    pub(in crate::native) ended: Option<bool>,
    pub(in crate::native) reason: String,
    pub(in crate::native) detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Exhaustion {
    Time,
    Bytes,
}

#[derive(Debug)]
enum ReadFault {
    Exhausted(Exhaustion),
    Busy,
    Unreadable(String),
}

/// What an Origin observation is told about the world outside the records: how a recorded
/// owner is attested and what time it is. One value per query; production attests through
/// the platform observer and reads the system clock, tests supply their own.
pub(in crate::native) struct OriginEnvironment<'a> {
    pub(in crate::native) attest: &'a dyn Fn(&NativeSessionOwner) -> ProcessObservation,
    pub(in crate::native) clock: &'a mut dyn Clock,
}

impl<'a> OriginEnvironment<'a> {
    pub(in crate::native) fn production(clock: &'a mut SystemClock) -> Self {
        Self {
            attest: &attest_through_platform_observer,
            clock,
        }
    }
}

fn attest_through_platform_observer(owner: &NativeSessionOwner) -> ProcessObservation {
    super::owner::attest_owner(owner).process
}

/// A cooperative deadline and a byte allowance shared by a sequence of record reads. The
/// clock is checked before and after each read; a synchronous filesystem call is not
/// interrupted, so the deadline is not a hard wall-clock guarantee.
pub(in crate::native) struct ReadBudget {
    deadline: Instant,
    remaining: u64,
    exhausted: Option<Exhaustion>,
}

impl ReadBudget {
    pub(in crate::native) fn new(deadline: Instant, bytes: u64) -> Self {
        Self {
            deadline,
            remaining: bytes,
            exhausted: None,
        }
    }

    /// The budget of one Origin observation inside a query, never later than `outer`.
    pub(in crate::native) fn for_query(outer: Option<Instant>, clock: &mut dyn Clock) -> Self {
        let local = clock.now() + OBSERVATION_TIME;
        Self::new(
            outer.map_or(local, |outer| outer.min(local)),
            OBSERVATION_BYTES,
        )
    }

    fn exhaust(&mut self, why: Exhaustion) -> ReadFault {
        self.exhausted.get_or_insert(why);
        ReadFault::Exhausted(why)
    }

    fn read(
        &mut self,
        record: &RecordReader,
        clock: &mut dyn Clock,
    ) -> Result<Option<Vec<u8>>, ReadFault> {
        if let Some(why) = self.exhausted {
            return Err(ReadFault::Exhausted(why));
        }
        if clock.now() >= self.deadline {
            return Err(self.exhaust(Exhaustion::Time));
        }
        if self.remaining == 0 {
            return Err(self.exhaust(Exhaustion::Bytes));
        }
        let limit = RECORD_LIMIT.min(self.remaining);
        let outcome = match record.timeline(limit) {
            Ok(None) => Ok(None),
            Ok(Some(bytes)) => {
                self.remaining -= bytes.len() as u64;
                Ok(Some(bytes))
            }
            Err(error) if error.is::<SnapshotBusy>() => Err(ReadFault::Busy),
            Err(error) => {
                // A record that fits the per-record limit but not what is left of the
                // allowance exhausts the budget; it is not a damaged record.
                let over_allowance = limit < RECORD_LIMIT
                    && fs::symlink_metadata(record.path()).is_ok_and(|metadata| {
                        metadata.is_file()
                            && metadata.len() > limit
                            && metadata.len() <= RECORD_LIMIT
                    });
                if over_allowance {
                    Err(self.exhaust(Exhaustion::Bytes))
                } else {
                    Err(ReadFault::Unreadable(format!("{error:#}")))
                }
            }
        };
        if outcome.is_err() {
            // The reader may have consumed up to `limit` bytes before it refused the record
            // (invalid UTF-8, growth, a busy read); reserve the whole attempt.
            self.remaining = self.remaining.saturating_sub(limit);
        }
        if outcome.is_ok() && clock.now() >= self.deadline {
            return Err(self.exhaust(Exhaustion::Time));
        }
        outcome
    }

    /// The Budgeted adapter over one core Record: the bytes within the budget, then the
    /// shared decoder. Only a decoder failure maps to `Unreadable`, malformed bytes with
    /// the path-qualified `invalid JSON in {path}` context and a broken rule with its own
    /// message; the read's own faults keep their kind and charging.
    fn decoded<T>(
        &mut self,
        record: &RecordReader,
        clock: &mut dyn Clock,
        decode: impl FnOnce(&[u8]) -> Result<T, DecodeError>,
    ) -> Result<Option<T>, ReadFault> {
        self.read(record, clock)?
            .map(|bytes| {
                decode(&bytes).map_err(|error| {
                    ReadFault::Unreadable(format!("{:#}", error.in_path(record.path())))
                })
            })
            .transpose()
    }
}

fn reference_with(
    kind: SourceKind,
    id: Option<String>,
    request_id: Option<String>,
    reason: &str,
    detail: Option<String>,
) -> OriginReference {
    OriginReference {
        id,
        kind,
        request_id,
        reason: reason.to_owned(),
        detail,
    }
}

fn unknown(request_id: Option<String>, reason: &str, detail: impl Into<String>) -> OriginReference {
    reference_with(
        SourceKind::Unknown,
        None,
        request_id,
        reason,
        Some(detail.into()),
    )
}

fn fault_reference(fault: ReadFault, reason: &str, what: &str) -> OriginReference {
    match fault {
        ReadFault::Exhausted(_) => unknown(
            None,
            "origin_budget_exhausted",
            format!("the read budget ended before the {what} was read"),
        ),
        ReadFault::Busy => unknown(
            None,
            "origin_busy",
            format!("the {what} changed while it was read"),
        ),
        ReadFault::Unreadable(error) => unknown(None, reason, format!("{what}: {error}")),
    }
}

/// The one selection rule: the launch Claim names the initial Request, and only the
/// receipt of exactly that Claim decides the Origin. No other receipt is a fallback.
fn select(
    own_id: &str,
    launch: Result<Option<launch::Record>, ReadFault>,
    receipt: impl FnOnce(&str) -> Result<Option<Receipt>, ReadFault>,
) -> OriginReference {
    let launch = match launch {
        Ok(Some(launch)) => launch,
        Ok(None) => {
            return unknown(
                None,
                "initial_request_unavailable",
                "no launch record names the initial request",
            );
        }
        Err(fault) => {
            return fault_reference(fault, "initial_request_unavailable", "launch record");
        }
    };
    if launch.schema != 1 || !valid_turn_claim_token(&launch.claim_token) {
        return unknown(
            None,
            "initial_request_unavailable",
            "the launch record has no valid claim token",
        );
    }
    let receipt = match receipt(&launch.claim_token) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            return unknown(
                None,
                "initial_request_unavailable",
                "no request receipt is recorded for the launch claim",
            );
        }
        Err(fault) => {
            return fault_reference(
                fault,
                "initial_receipt_unreadable",
                "initial request receipt",
            );
        }
    };
    let request_id = Some(receipt.request_id);
    match receipt.source.as_deref() {
        None => unknown(
            request_id,
            "source_unavailable",
            "the initial request receipt records no source",
        ),
        Some(EXTERNAL_SOURCE) => reference_with(
            SourceKind::External,
            None,
            request_id,
            "origin_external",
            None,
        ),
        Some(source) if source == own_id => unknown(
            request_id,
            "invalid_source",
            "the initial request names this session as its own source",
        ),
        Some(source) if !valid_session_id(source) => unknown(
            request_id,
            "invalid_source",
            "the recorded source is not a session id",
        ),
        Some(source) => reference_with(
            SourceKind::Managed,
            Some(source.to_owned()),
            request_id,
            "recorded_source",
            None,
        ),
    }
}

/// The Origin of a session whose consistent Snapshot was already read.
pub(in crate::native) fn reference(records: &Snapshot) -> OriginReference {
    select(&records.manifest.id, Ok(records.launch.clone()), |token| {
        if let Some(receipt) = records.receipts.iter().find(|r| r.claim_token == token) {
            return Ok(Some(receipt.clone()));
        }
        if records.unreadable_requests > 0 || records.request_index_error.is_some() {
            return Err(ReadFault::Unreadable(
                "the request index is incomplete; the initial receipt may be one of the unreadable records"
                    .to_owned(),
            ));
        }
        Ok(None)
    })
}

/// The Origin of a session read directly: its launch record and the one receipt of the
/// launch Claim, each within `budget`. No index, event or lock is read.
pub(in crate::native) fn read_reference(
    reader: &Reader,
    budget: &mut ReadBudget,
    clock: &mut dyn Clock,
) -> OriginReference {
    let own_id = reader
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let launch = budget.decoded(&reader.record(CoreRecord::Launch), clock, record::launch);
    select(&own_id, launch, |token| {
        if !requests::requests_directory_present(reader)
            .map_err(|error| ReadFault::Unreadable(format!("{error:#}")))?
        {
            return Ok(None);
        }
        #[cfg(test)]
        tests::count_receipt_read();
        budget.decoded(&requests::receipt_record(reader, token), clock, |bytes| {
            record::receipt(bytes, token)
        })
    })
}

#[derive(Clone, Debug, PartialEq)]
struct Target {
    stored_state: Option<String>,
    ended: Option<bool>,
    reason: &'static str,
    detail: Option<String>,
}

fn target(ended: Option<bool>, reason: &'static str, detail: Option<String>) -> Target {
    Target {
        stored_state: None,
        ended,
        reason,
        detail,
    }
}

fn fault_target(fault: ReadFault, unreadable: &'static str) -> Target {
    match fault {
        ReadFault::Exhausted(_) => target(None, "origin_budget_exhausted", None),
        ReadFault::Busy => target(None, "origin_busy", None),
        ReadFault::Unreadable(error) => target(None, unreadable, Some(error)),
    }
}

/// Observe one managed Origin in `root`. External and unknown references are returned
/// without reading anything.
pub(in crate::native) fn observe(
    root: &Path,
    reference: OriginReference,
    budget: &mut ReadBudget,
    environment: &mut OriginEnvironment<'_>,
) -> OriginObservation {
    finish(reference, |id| {
        observe_target(root, id, budget, environment)
    })
}

fn finish(reference: OriginReference, observe: impl FnOnce(&str) -> Target) -> OriginObservation {
    let observed = match (reference.kind, reference.id.as_deref()) {
        (SourceKind::Managed, Some(id)) => observe(id),
        (SourceKind::External, _) => target(None, "origin_external", None),
        _ => {
            let reason = reference.reason.clone();
            let detail = reference.detail.clone();
            return OriginObservation {
                reference,
                stored_state: None,
                ended: None,
                reason,
                detail,
            };
        }
    };
    OriginObservation {
        reference,
        stored_state: observed.stored_state,
        ended: observed.ended,
        reason: observed.reason.to_owned(),
        detail: observed.detail,
    }
}

/// Observations of one query response. The same Origin is read once per response;
/// nothing is kept between queries.
#[derive(Default)]
pub(in crate::native) struct ObservationCache {
    targets: HashMap<String, Target>,
}

impl ObservationCache {
    pub(in crate::native) fn observe(
        &mut self,
        root: &Path,
        reference: OriginReference,
        outer: Option<Instant>,
        environment: &mut OriginEnvironment<'_>,
    ) -> OriginObservation {
        finish(reference, |id| {
            self.targets
                .entry(id.to_owned())
                .or_insert_with(|| {
                    // A target read in this response constructs no budget and samples
                    // no clock.
                    let mut budget = ReadBudget::for_query(outer, environment.clock);
                    observe_target(root, id, &mut budget, environment)
                })
                .clone()
        })
    }
}

fn session_directory(root: &Path, id: &str) -> Result<Option<Reader>, Target> {
    if !valid_session_id(id) {
        return Err(target(None, "invalid_source", None));
    }
    let directory = root.join(id);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(target(
            None,
            "origin_unreadable",
            Some("refusing a linked or non-directory session path".to_owned()),
        )),
        Ok(_) => Ok(Some(Reader::open_unchecked(directory))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(target(None, "origin_unreadable", Some(error.to_string()))),
    }
}

fn bound_manifest(
    reader: &Reader,
    id: &str,
    budget: &mut ReadBudget,
    clock: &mut dyn Clock,
) -> Result<(), ReadFault> {
    // The decoder's schema and id rules are Origin's one binding rule, in its own words.
    budget
        .decoded(&reader.record(CoreRecord::Manifest), clock, |bytes| {
            record::manifest(bytes, id).map_err(|error| match error {
                DecodeError::Rule(_) => DecodeError::Rule(anyhow::anyhow!(
                    "the session manifest does not bind this session id"
                )),
                malformed => malformed,
            })
        })?
        .ok_or_else(|| ReadFault::Unreadable("the session has no manifest".to_owned()))?;
    Ok(())
}

fn status_of(bytes: &[u8]) -> Result<SessionStatus, ReadFault> {
    record::status(bytes)
        .map_err(|error| ReadFault::Unreadable(format!("invalid status record: {error}")))
}

fn observe_target(
    root: &Path,
    id: &str,
    budget: &mut ReadBudget,
    environment: &mut OriginEnvironment<'_>,
) -> Target {
    let reader = match session_directory(root, id) {
        Ok(Some(reader)) => reader,
        Ok(None) => return target(None, "origin_missing", None),
        Err(target) => return target,
    };
    let OriginEnvironment { attest, clock } = environment;
    let clock: &mut dyn Clock = &mut **clock;
    if let Err(fault) = bound_manifest(&reader, id, budget, clock) {
        return fault_target(fault, "origin_unreadable");
    }
    match budget.read(&reader.record(CoreRecord::Closed), clock) {
        Err(fault) => return fault_target(fault, "origin_unreadable"),
        Ok(Some(bytes)) => {
            return match status_of(&bytes) {
                Ok(closed) if closed.state == SessionState::Closed => {
                    // The tombstone is the close's commit; a status that has not caught up
                    // does not undo it, and reading the tombstone repairs nothing.
                    let status = budget
                        .read(&reader.record(CoreRecord::Status), clock)
                        .and_then(|bytes| bytes.map(|bytes| status_of(&bytes)).transpose());
                    let (stored_state, detail) = match status {
                        Ok(Some(status)) if status.state == SessionState::Closed => {
                            (Some(status.state.as_str().to_owned()), None)
                        }
                        Ok(Some(status)) => (
                            Some(status.state.as_str().to_owned()),
                            Some(format!(
                                "the close tombstone is recorded; the status record still says {}",
                                status.state
                            )),
                        ),
                        Ok(None) => (
                            None,
                            Some("the close tombstone is recorded; the status record is missing".to_owned()),
                        ),
                        Err(_) => (
                            None,
                            Some("the close tombstone is recorded; the status record could not be read".to_owned()),
                        ),
                    };
                    Target {
                        stored_state,
                        ended: Some(true),
                        reason: "recorded_closed",
                        detail,
                    }
                }
                Ok(_) => target(
                    None,
                    "origin_unreadable",
                    Some("the close tombstone does not record a closed state".to_owned()),
                ),
                Err(fault) => fault_target(fault, "origin_unreadable"),
            };
        }
        Ok(None) => (),
    }
    let status_bytes = match budget.read(&reader.record(CoreRecord::Status), clock) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return target(
                None,
                "origin_unreadable",
                Some("the session has no status record".to_owned()),
            );
        }
        Err(fault) => return fault_target(fault, "origin_unreadable"),
    };
    let status = match status_of(&status_bytes) {
        Ok(status) => status,
        Err(fault) => return fault_target(fault, "origin_unreadable"),
    };
    // The owner record a verdict was derived from: its bytes, or `Some(None)` for "no
    // owner record". `None` when no owner-derived verdict depends on it.
    let mut owner_bytes: Option<Option<Vec<u8>>> = None;
    let (ended, reason, detail) = match &status.state {
        SessionState::Closed => (Some(true), "recorded_closed", None),
        SessionState::Failed => (Some(true), "recorded_failed", None),
        SessionState::Exited => (Some(true), "recorded_exited", None),
        SessionState::Unknown(_) => (
            None,
            "origin_unverified",
            Some("the recorded state is not one this version knows".to_owned()),
        ),
        _ => {
            let (owner, read) = owner_verdict(&reader, id, budget, clock, &**attest);
            owner_bytes = read;
            (owner.ended, owner.reason, owner.detail)
        }
    };
    #[cfg(test)]
    tests::checkpoint(tests::Checkpoint::OriginRead, reader.directory());
    // The verdict holds only if the tombstone is still absent, the status unchanged, and
    // the owner record it was derived from unchanged or still absent.
    match budget.read(&reader.record(CoreRecord::Closed), clock) {
        Ok(None) => (),
        Ok(Some(_)) => return target(None, "origin_busy", None),
        Err(fault) => return fault_target(fault, "origin_unreadable"),
    }
    match budget.read(&reader.record(CoreRecord::Status), clock) {
        Ok(Some(bytes)) if bytes == status_bytes => (),
        Ok(_) => return target(None, "origin_busy", None),
        Err(fault) => return fault_target(fault, "origin_unreadable"),
    }
    if let Some(owner_bytes) = owner_bytes {
        match budget.read(&reader.record(CoreRecord::Owner), clock) {
            Ok(bytes) if bytes == owner_bytes => (),
            Ok(_) => return target(None, "origin_busy", None),
            Err(fault) => return fault_target(fault, "origin_unreadable"),
        }
    }
    Target {
        stored_state: Some(status.state.as_str().to_owned()),
        ended,
        reason,
        detail,
    }
}

/// The owner verdict and the owner record bytes it was derived from (`Some(None)` when
/// there was no owner record, `None` when the record could not be read).
fn owner_verdict(
    reader: &Reader,
    id: &str,
    budget: &mut ReadBudget,
    clock: &mut dyn Clock,
    attest: &dyn Fn(&NativeSessionOwner) -> ProcessObservation,
) -> (Target, Option<Option<Vec<u8>>>) {
    let record = reader.record(CoreRecord::Owner);
    let bytes = match budget.read(&record, clock) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return (
                target(None, "owner_unverified", Some("no owner record".to_owned())),
                Some(None),
            );
        }
        Err(fault) => return (fault_target(fault, "owner_unverified"), None),
    };
    let verdict = match serde_json::from_slice::<NativeSessionOwner>(&bytes) {
        Ok(owner) => owner_attested(&owner, id, attest),
        Err(error) => target(
            None,
            "owner_unverified",
            Some(format!(
                "invalid JSON in {}: {error}",
                record.path().display()
            )),
        ),
    };
    (verdict, Some(Some(bytes)))
}

fn owner_attested(
    owner: &NativeSessionOwner,
    id: &str,
    attest: &dyn Fn(&NativeSessionOwner) -> ProcessObservation,
) -> Target {
    match owner.managed_session_id.as_deref() {
        Some(bound) if bound == id => (),
        None => {
            return target(
                None,
                "owner_unverified",
                Some("owner record is unbound".to_owned()),
            );
        }
        Some(_) => {
            return target(
                None,
                "owner_unverified",
                Some("owner record belongs to another session".to_owned()),
            );
        }
    }
    // Only the attested process decides: the projected liveness bools would turn a reused
    // pid or a changed control identity into a death, and an Unknown into a guess.
    match attest(owner) {
        ProcessObservation::Gone(Gone::Exited) => target(Some(true), "owner_exited", None),
        ProcessObservation::Gone(Gone::Reused(reason)) => target(
            Some(true),
            "owner_reused",
            Some(format!(
                "{reason}; the new process grants no authority over the session"
            )),
        ),
        ProcessObservation::Live(Attested::Incarnation) => target(Some(false), "owner_live", None),
        ProcessObservation::Live(Attested::PidOnly) => target(
            Some(false),
            "owner_live",
            Some(
                "pid-only owner record: a live process id does not attest the recorded incarnation"
                    .to_owned(),
            ),
        ),
        ProcessObservation::Unknown(reason) => target(None, "owner_unverified", Some(reason)),
    }
}

impl OriginObservation {
    /// The public `origin_observation` object, identical in every query.
    pub(in crate::native) fn value(&self) -> Value {
        json!({
            "kind": self.reference.kind,
            "request_id": self.reference.request_id,
            "stored_state": self.stored_state,
            "ended": self.ended,
            "reason": self.reason,
            "detail": self.detail,
            "derived_from": "records",
        })
    }

    /// Adds `origin` and `origin_observation` to a query object.
    pub(in crate::native) fn add_fields(&self, target: &mut Value) {
        target["origin"] = json!(self.reference.id);
        target["origin_observation"] = self.value();
    }
}

#[derive(Debug, Serialize)]
pub(in crate::native) struct IncompleteReason {
    pub(in crate::native) session: Option<String>,
    pub(in crate::native) reason: String,
}

#[derive(Debug, Default, Serialize)]
pub(in crate::native) struct Scanned {
    pub(in crate::native) sessions: u64,
    pub(in crate::native) listed: u64,
}

/// Sessions whose initial Request names the closed session and that are not closed in the
/// observed records. Not a snapshot of the registry: sessions can appear, close or change
/// during and after the scan.
#[derive(Debug, Default, Serialize)]
pub(in crate::native) struct StartedFrom {
    pub(in crate::native) open: Vec<String>,
    pub(in crate::native) incomplete: bool,
    pub(in crate::native) incomplete_reasons: Vec<IncompleteReason>,
    pub(in crate::native) additional_incomplete_reasons: u64,
    pub(in crate::native) scanned: Scanned,
}

impl StartedFrom {
    fn note(&mut self, session: Option<String>, reason: String) {
        self.incomplete = true;
        if self.incomplete_reasons.len() < SCAN_REASONS {
            self.incomplete_reasons
                .push(IncompleteReason { session, reason });
        } else {
            self.additional_incomplete_reasons += 1;
        }
    }
}

enum Examined {
    Unrelated,
    Open,
    Incomplete(String),
    Stop(&'static str),
}

fn stop(why: Exhaustion) -> Examined {
    Examined::Stop(match why {
        Exhaustion::Time => "time_budget_exhausted",
        Exhaustion::Bytes => "byte_limit",
    })
}

fn scan_fault(fault: ReadFault, unreadable: &str) -> Examined {
    match fault {
        ReadFault::Exhausted(why) => stop(why),
        ReadFault::Busy => Examined::Incomplete("session_changed".to_owned()),
        ReadFault::Unreadable(error) => Examined::Incomplete(format!("{unreadable}: {error}")),
    }
}

fn closed_state(bytes: Option<&[u8]>) -> Option<bool> {
    bytes.map(|bytes| status_of(bytes).is_ok_and(|s| s.state == SessionState::Closed))
}

fn examine(
    reader: &Reader,
    id: &str,
    origin_id: &str,
    budget: &mut ReadBudget,
    clock: &mut dyn Clock,
) -> Examined {
    if let Err(fault) = bound_manifest(reader, id, budget, clock) {
        return scan_fault(fault, "records_unreadable");
    }
    let closed = match budget.read(&reader.record(CoreRecord::Closed), clock) {
        Ok(closed) => closed,
        Err(fault) => return scan_fault(fault, "records_unreadable"),
    };
    let status = match budget.read(&reader.record(CoreRecord::Status), clock) {
        Ok(status) => status,
        Err(fault) => return scan_fault(fault, "records_unreadable"),
    };
    match closed_state(closed.as_deref()) {
        Some(true) if closed_state(status.as_deref()) == Some(true) => return Examined::Unrelated,
        Some(true) => return Examined::Incomplete("close_in_progress".to_owned()),
        Some(false) => {
            return Examined::Incomplete(
                "records_unreadable: the close tombstone does not record a closed state".to_owned(),
            );
        }
        None => (),
    }
    let Some(status_bytes) = status else {
        return Examined::Incomplete(
            "records_unreadable: the session has no status record".to_owned(),
        );
    };
    match status_of(&status_bytes) {
        Ok(status) if status.state == SessionState::Closed => return Examined::Unrelated,
        Ok(status) if matches!(status.state, SessionState::Unknown(_)) => {
            return Examined::Incomplete(
                "records_unreadable: the recorded state is not one this version knows".to_owned(),
            );
        }
        Ok(_) => (),
        Err(fault) => return scan_fault(fault, "records_unreadable"),
    }
    let reference = read_reference(reader, budget, clock);
    if let Some(why) = budget.exhausted {
        return stop(why);
    }
    match (reference.kind, reference.id.as_deref()) {
        (SourceKind::Managed, Some(source)) if source == origin_id => (),
        (SourceKind::Managed | SourceKind::External, _) => return Examined::Unrelated,
        (SourceKind::Unknown, _) => {
            return Examined::Incomplete(match reference.detail {
                Some(detail) => format!("{}: {detail}", reference.reason),
                None => reference.reason,
            });
        }
    }
    #[cfg(test)]
    tests::checkpoint(tests::Checkpoint::Matched, reader.directory());
    // Recheck just before reporting: a session that closed since is not open.
    let closed = match budget.read(&reader.record(CoreRecord::Closed), clock) {
        Ok(closed) => closed,
        Err(fault) => return scan_fault(fault, "session_changed"),
    };
    match closed_state(closed.as_deref()) {
        Some(true) => return Examined::Unrelated,
        Some(false) => {
            return Examined::Incomplete(
                "session_changed: the close tombstone does not record a closed state".to_owned(),
            );
        }
        None => (),
    }
    match budget.read(&reader.record(CoreRecord::Status), clock) {
        Ok(Some(bytes)) if bytes == status_bytes => Examined::Open,
        Ok(Some(bytes)) if closed_state(Some(&bytes)) == Some(true) => Examined::Unrelated,
        Ok(_) => Examined::Incomplete("session_changed".to_owned()),
        Err(fault) => scan_fault(fault, "session_changed"),
    }
}

/// One bounded pass over the state root after `origin_id` was closed. Lazy enumeration,
/// at most `SCAN_ENTRIES` sessions, `RECORD_LIMIT` per record and `SCAN_BYTES` in all;
/// per session only the manifest, the tombstone and status, and for a session that is not
/// closed its launch record and the one receipt of the launch Claim. Writes nothing.
pub(in crate::native) fn scan_started_from(
    root: &Path,
    origin_id: &str,
    deadline: Instant,
    clock: &mut dyn Clock,
) -> StartedFrom {
    let mut scan = StartedFrom::default();
    if clock.now() >= deadline {
        scan.note(None, "time_budget_exhausted".to_owned());
        return scan;
    }
    let mut budget = ReadBudget::new(deadline, SCAN_BYTES);
    let entries = match enumerate(root, CheckOrder::IdFirst) {
        Ok(entries) => entries,
        Err(error) => {
            scan.note(None, format!("root_unreadable: {error}"));
            return scan;
        }
    };
    for entry in entries {
        if clock.now() >= deadline {
            scan.note(None, "time_budget_exhausted".to_owned());
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(Fault::Entry(error)) => {
                scan.note(None, format!("entry_unreadable: {error}"));
                continue;
            }
            Err(Fault::FileType { id, error }) => {
                scan.note(Some(id), format!("entry_unreadable: {error}"));
                continue;
            }
        };
        if entry.id == origin_id {
            continue;
        }
        scan.scanned.sessions += 1;
        #[cfg(test)]
        tests::checkpoint(tests::Checkpoint::Entry, &entry.directory);
        let reader = Reader::open_unchecked(&entry.directory);
        match examine(&reader, &entry.id, origin_id, &mut budget, clock) {
            Examined::Unrelated => (),
            Examined::Open => {
                scan.open.push(entry.id);
                if scan.open.len() >= SCAN_OPEN {
                    scan.note(
                        None,
                        format!("open_limit: stopped after {SCAN_OPEN} sessions"),
                    );
                    break;
                }
            }
            Examined::Incomplete(reason) => scan.note(Some(entry.id), reason),
            Examined::Stop(reason) => {
                scan.note(None, reason.to_owned());
                break;
            }
        }
        if scan.scanned.sessions >= SCAN_ENTRIES {
            scan.note(
                None,
                format!("entry_limit: stopped after {SCAN_ENTRIES} sessions"),
            );
            break;
        }
    }
    scan.open.sort();
    scan.scanned.listed = scan.open.len() as u64;
    scan
}

#[cfg(test)]
pub(in crate::native) mod tests;
