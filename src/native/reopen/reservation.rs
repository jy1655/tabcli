//! Reopen's exclusive source reservation and launch-refusal settlement.
//! A finalized marker is consumed until the refused provider is proven gone; queries only observe.
use super::gate::{ReopenGate, reopen_refusal, reopen_refusal_gate};
use crate::native::session::primitives::{unix_ms, valid_session_id};
use crate::native::session::schema::{ProviderProcessRecord, RecordedReopenRefusal, ReopenMarker};
use crate::native::session::turn::TURN_CLAIM_SEQUENCE;
use crate::native::session::{
    self, CoreRecord, Reader, RecordReader, RecordStore, SessionState, Store, record,
};
use crate::native::terminal;
use agent_bridge::process_is_alive;
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

mod marker;
pub(in crate::native) use marker::check as marker_check;

// Only launch gates persist refusals across processes; a later tell refusal must never
// authorize retry of an uncertain initial delivery.
const REOPEN_REFUSAL_LAUNCH_PHASE: &str = "launch";

// Cleanup is a diagnostic for doctor and the next reopen; only live records decide release.
const REOPEN_REFUSAL_CLEANUP_PENDING: &str = "pending";

// Records a launch-phase gate refusal that happened after the new session existed, then
// returns the typed refusal. Only the launch phase writes this record; follow-up refusals
// go through `record_follow_up_refusal`, which touches the session status alone.
pub(in crate::native) fn record_reopen_refusal(
    directory: &Path,
    gate: ReopenGate,
    detail: String,
) -> anyhow::Error {
    let record = RecordedReopenRefusal {
        schema: 2,
        phase: REOPEN_REFUSAL_LAUNCH_PHASE.to_owned(),
        gate: gate.as_str().to_owned(),
        detail: detail.clone(),
        created_unix_ms: unix_ms(),
        claim_token: None,
        cleanup: None,
        cleanup_detail: None,
    };
    if let Err(error) = Store::open_unchecked(directory).write_reopen_refusal(&record) {
        return reopen_refusal(
            gate,
            format!("{detail}; the refusal record could not be written: {error:#}"),
        );
    }
    reopen_refusal(gate, detail)
}

// The launch-phase refusal recorded in a reopened session, if there is one. A record of any
// other schema or phase is not a launch refusal and yields nothing, so the caller treats the
// launch as not refused.
fn read_reopen_launch_refusal(directory: &Path) -> Option<RecordedReopenRefusal> {
    let text = Reader::open_unchecked(directory)
        .record(CoreRecord::ReopenRefusal)
        .text()
        .ok()
        .flatten()?;
    decode_launch_refusal(&text)
}

fn decode_launch_refusal(text: &str) -> Option<RecordedReopenRefusal> {
    let record: RecordedReopenRefusal = record::reopen_refusal(text).ok()?;
    let valid = record.phase == REOPEN_REFUSAL_LAUNCH_PHASE
        && match record.schema {
            2 => {
                record.gate != ReopenGate::LaunchFailed.as_str()
                    && ReopenGate::from_wire(&record.gate).is_some_and(ReopenGate::is_post_creation)
            }
            3 => {
                record.gate == ReopenGate::LaunchFailed.as_str()
                    && record
                        .claim_token
                        .as_ref()
                        .is_some_and(|token| !token.trim().is_empty())
            }
            _ => false,
        };
    valid.then_some(record)
}

pub(super) fn record_failed_launch(
    directory: &Path,
    token: &str,
    error: &anyhow::Error,
) -> Result<ReopenGate> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    if let Some(text) = store.record(CoreRecord::ReopenRefusal).text()? {
        let record =
            decode_launch_refusal(&text).context("the existing launch refusal is invalid")?;
        if record.gate == ReopenGate::LaunchFailed.as_str()
            && record.claim_token.as_deref() != Some(token)
        {
            bail!("the existing launch refusal belongs to another claim");
        }
        return Ok(ReopenGate::from_wire(&record.gate).expect("validated launch gate"));
    }
    let gate = reopen_refusal_gate(error)
        .filter(|gate| gate.is_post_creation())
        .unwrap_or(ReopenGate::LaunchFailed);
    let record = RecordedReopenRefusal {
        schema: if gate == ReopenGate::LaunchFailed {
            3
        } else {
            2
        },
        phase: REOPEN_REFUSAL_LAUNCH_PHASE.to_owned(),
        gate: gate.as_str().to_owned(),
        detail: format!("{error:#}"),
        created_unix_ms: unix_ms(),
        claim_token: (gate == ReopenGate::LaunchFailed).then(|| token.to_owned()),
        cleanup: None,
        cleanup_detail: None,
    };
    store
        .record(CoreRecord::ReopenRefusal)
        .write_private(&serde_json::to_vec_pretty(&record)?)?;
    Ok(gate)
}

fn read_reopen_refusal_gate(directory: &Path) -> Option<ReopenGate> {
    read_reopen_launch_refusal(directory).and_then(|record| ReopenGate::from_wire(&record.gate))
}

// Notes in the launch refusal record that the marker settlement could not establish that
// the refused launch is gone. A missing record (its write failed when the refusal was
// decided) leaves nothing to annotate; the retention itself does not depend on the note.
fn record_reopen_refusal_cleanup_pending(directory: &Path, reason: &str) -> Result<()> {
    let Some(mut record) = read_reopen_launch_refusal(directory) else {
        return Ok(());
    };
    record.cleanup = Some(REOPEN_REFUSAL_CLEANUP_PENDING.to_owned());
    record.cleanup_detail = Some(reason.to_owned());
    Store::open_unchecked(directory).write_reopen_refusal(&record)
}

// Why a recorded provider process is known to be gone. Identity mismatches are only
// observable where the reopen slice runs (native Windows); other targets never build one.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessGone {
    // Its pid is no longer alive.
    Exited,
    // Its pid is alive but belongs to a different process: the pid was reused after the
    // recorded process exited (Windows creation time or executable path differ).
    IdentityMismatch(String),
}

// What can be observed about a recorded provider process now.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessObservation {
    Gone(ProviderProcessGone),
    // The pid is alive and, where an identity was recorded, still carries it.
    Alive,
    // Its identity could not be inspected; PID-only liveness cannot establish whether
    // the recorded incarnation survived, exited, or was reused.
    Unknown(String),
}

// A verified PID reuse releases the marker; an inspection failure retains it.
fn observe_provider_process(record: &ProviderProcessRecord) -> ProviderProcessObservation {
    #[cfg(windows)]
    // B2: an exited provider whose object is still retained is Gone(Exited), as the former
    // typed check also concluded through its liveness fallback; no liveness bool is consulted.
    return observe_provider_process_with(record, terminal::observe_windows_process_identity);
    #[cfg(not(windows))]
    {
        use crate::native::process_identity::{self, ProcessObservation};
        match process_identity::classify::<()>(
            record.pid,
            None,
            || process_is_alive(record.pid),
            || Ok(None),
        ) {
            ProcessObservation::Live(_) => ProviderProcessObservation::Alive,
            ProcessObservation::Gone(_) => {
                ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
            }
            ProcessObservation::Unknown(reason) => ProviderProcessObservation::Unknown(reason),
        }
    }
}

#[cfg(any(windows, test))]
fn observe_provider_process_with(
    record: &ProviderProcessRecord,
    observe: impl FnOnce(u32) -> Result<Option<terminal::WindowsProcessIdentity>>,
) -> ProviderProcessObservation {
    use crate::native::process_identity::{self, Gone, ProcessObservation};
    use terminal::WindowsIncarnation;
    let recorded = record
        .windows_process_identity
        .clone()
        .map(WindowsIncarnation);
    // Keep the published Windows mismatch vocabulary while classify owns the decision.
    let mut mismatch = "Windows console process executable identity changed";
    let observation = process_identity::classify(
        record.pid,
        recorded.as_ref(),
        || process_is_alive(record.pid),
        || {
            let live = observe(record.pid)?;
            if let (Some(recorded), Some(live)) = (&recorded, &live)
                && recorded.0.creation_time != live.creation_time
            {
                mismatch = "Windows console process id was reused";
            }
            Ok(live.map(WindowsIncarnation))
        },
    );
    match observation {
        ProcessObservation::Live(_) => ProviderProcessObservation::Alive,
        ProcessObservation::Gone(Gone::Exited) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
        }
        ProcessObservation::Gone(Gone::Reused(_)) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::IdentityMismatch(mismatch.into()))
        }
        ProcessObservation::Unknown(reason) => ProviderProcessObservation::Unknown(reason),
    }
}

// Release only when the provider is verified gone or never spawned. Wrapper death and
// a closed surface cannot rule out an unregistered provider that still holds the conversation.
#[derive(Debug, PartialEq)]
enum RefusedLaunchCleanup {
    // The refusal came before a provider process could exist (the pre-spawn recheck, or a
    // failure before the launch) and none was recorded: the launch wrapper started none.
    NoProcessSpawned,
    // The provider process the launch wrapper recorded is verified gone: its pid is dead
    // or the pid is alive under a different identity. `surface_closed` adds that the
    // refused session's surface was consumed by a close; it is reported, never relied on.
    ProviderProcessGone {
        pid: u32,
        evidence: ProviderProcessGone,
        surface_closed: bool,
    },
    // Neither of the above can be established from the session's records.
    Pending(String),
}

impl RefusedLaunchCleanup {
    fn releases_marker(&self) -> bool {
        !matches!(self, Self::Pending(_))
    }
}

impl std::fmt::Display for RefusedLaunchCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProcessSpawned => write!(formatter, "no provider process was spawned"),
            Self::ProviderProcessGone {
                pid,
                evidence,
                surface_closed,
            } => {
                write!(formatter, "provider process {pid} is verified gone (")?;
                match evidence {
                    ProviderProcessGone::Exited => write!(formatter, "it has exited")?,
                    ProviderProcessGone::IdentityMismatch(reason) => {
                        write!(
                            formatter,
                            "the pid now belongs to another process: {reason}"
                        )?;
                    }
                }
                write!(formatter, ")")?;
                if *surface_closed {
                    write!(formatter, " and the refused session's surface was closed")?;
                }
                Ok(())
            }
            Self::Pending(reason) => {
                write!(
                    formatter,
                    "the refused launch may still hold the conversation: {reason}"
                )
            }
        }
    }
}

// Read-only: ready/working sessions are not refused launches. A pre-spawn refusal proves
// no spawn only without a provider record; an existing record must be verified.
fn refused_launch_cleanup(
    refused_directory: &Path,
    refusal: &RecordedReopenRefusal,
) -> Result<RefusedLaunchCleanup> {
    refused_launch_cleanup_with(refused_directory, refusal, observe_provider_process)
}

fn refused_launch_cleanup_with(
    refused_directory: &Path,
    refusal: &RecordedReopenRefusal,
    observe: impl FnOnce(&ProviderProcessRecord) -> ProviderProcessObservation,
) -> Result<RefusedLaunchCleanup> {
    let store = Store::open_unchecked(refused_directory);
    let generic = refusal.gate == ReopenGate::LaunchFailed.as_str();
    let _lock = if generic {
        let Some(lock) = store.try_lock()? else {
            return Ok(RefusedLaunchCleanup::Pending(
                "provider startup holds the lifecycle lock".to_owned(),
            ));
        };
        Some(lock)
    } else {
        None
    };
    let evidence = if generic {
        let token = refusal
            .claim_token
            .as_deref()
            .context("the launch refusal has no claim token")?;
        let evidence = session::launch::failed_start_evidence_locked(&store, token)?;
        if let session::launch::FailedStartEvidence::Pending(reason) = evidence {
            return Ok(RefusedLaunchCleanup::Pending(reason));
        }
        Some(evidence)
    } else {
        None
    };
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    // The refused session's own status decides whether cleanup is even possible, so it is
    // read like the other evidence here: a link at `status.json` is refused, not followed.
    let status_path = Reader::open_unchecked(refused_directory)
        .record(CoreRecord::Status)
        .path()
        .to_owned();
    let state = Reader::open_unchecked(refused_directory)
        .status_if_present()?
        .with_context(|| format!("failed to read {}", status_path.display()))?
        .state;
    if state.accepts_prompt() || state == SessionState::Working {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state}"
        )));
    }
    let record = Reader::open_unchecked(refused_directory).provider_process_if_present()?;
    let Some(record) = record else {
        if refusal.gate == ReopenGate::ProviderUnsupported.as_str()
            || refusal.gate == ReopenGate::StartFailed.as_str()
            || matches!(
                evidence,
                Some(session::launch::FailedStartEvidence::NoUnresolvedSpawn)
            )
        {
            return Ok(RefusedLaunchCleanup::NoProcessSpawned);
        }
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state} with no provider process record ({}), so the provider process it spawned cannot be verified gone",
            CoreRecord::ProviderProcess.name()
        )));
    };
    if record.schema != 1 || record.managed_session_id != refused_session {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "the provider process record of refused session {refused_session} names {:?} (schema {})",
            record.managed_session_id, record.schema
        )));
    }
    let evidence = match observe(&record) {
        ProviderProcessObservation::Gone(evidence) => evidence,
        ProviderProcessObservation::Alive => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} is still running",
                record.pid
            )));
        }
        ProviderProcessObservation::Unknown(error) => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} could not be verified: {error}",
                record.pid
            )));
        }
    };
    let surface_closed = state == SessionState::Closed
        && RecordReader::at(
            Reader::open_unchecked(refused_directory)
                .record(CoreRecord::TerminalClosed)
                .path(),
        )
        .is_regular_file()?
        && Reader::open_unchecked(refused_directory)
            .closed_if_present()?
            .is_some_and(|closed| closed.state == SessionState::Closed);
    Ok(RefusedLaunchCleanup::ProviderProcessGone {
        pid: record.pid,
        evidence,
        surface_closed,
    })
}

// Only a launch-phase refusal can release the source marker. A later tell can fail while
// the initial messenger settles; its refusal cannot prove the initial prompt was not sent.
pub(super) fn settle_reopen_outcome(
    source_directory: &Path,
    session_directory: &Path,
    outcome: Result<()>,
) -> (Result<()>, Option<ReopenGate>) {
    // Retain the directories selected by this attempt, but repeat the original
    // session-root validation before reading a refusal and before any settlement write.
    let revalidate = |directory: &Path| {
        let root = directory
            .parent()
            .context("session directory has no state root")?;
        let id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("session directory has no valid name")?;
        Reader::session_directory_in(root, id)
    };
    let gate = outcome.as_ref().err().and_then(|error| {
        reopen_refusal_gate(error).or_else(|| {
            revalidate(session_directory)
                .ok()
                .and_then(|directory| read_reopen_refusal_gate(&directory))
        })
    });
    let outcome = match gate {
        Some(gate) if gate.is_post_creation() => {
            match (revalidate(source_directory), revalidate(session_directory)) {
                (Ok(source_directory), Ok(refused_directory)) => {
                    release_reopen_marker_after_refusal(
                        &source_directory,
                        &refused_directory,
                        gate,
                        outcome,
                    )
                }
                (Err(error), _) | (_, Err(error)) => outcome.context(format!(
                    "the reopen marker of source session {} was not released: {error:#}",
                    source_directory
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default(),
                )),
            }
        }
        _ => outcome,
    };
    (outcome, gate)
}

pub(super) fn retry_interrupted_launch_fence(source_directory: &Path) -> Result<()> {
    let marker = Reader::open_unchecked(source_directory).reopen_marker_if_present();
    let Ok(Some(marker)) = marker else {
        return Ok(());
    };
    let Some(child) = marker.reopened_by.filter(|id| valid_session_id(id)) else {
        return Ok(());
    };
    let child = source_directory
        .parent()
        .context("session directory has no state root")?
        .join(child);
    if !fs::symlink_metadata(&child).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(());
    }
    let Some(refusal) = read_reopen_launch_refusal(&child) else {
        return Ok(());
    };
    if refusal.gate == ReopenGate::LaunchFailed.as_str() {
        session::launch::retry_failed_start_fence(
            &Store::open_unchecked(&child),
            refusal
                .claim_token
                .as_deref()
                .expect("validated launch token"),
            &refusal.detail,
        )?;
    }
    Ok(())
}

// Release only the marker naming this refused launch, once its provider is verified gone.
// Keep resumed_from; pending cleanup retains the marker for the next reopen to reconcile.
fn release_reopen_marker_after_refusal(
    source_directory: &Path,
    refused_directory: &Path,
    gate: ReopenGate,
    outcome: Result<()>,
) -> Result<()> {
    release_reopen_marker_after_refusal_with(source_directory, refused_directory, outcome, || {
        let refusal = read_reopen_launch_refusal(refused_directory);
        let refusal = match refusal {
            Some(refusal) => refusal,
            None if gate != ReopenGate::LaunchFailed => RecordedReopenRefusal {
                schema: 2,
                phase: REOPEN_REFUSAL_LAUNCH_PHASE.to_owned(),
                gate: gate.as_str().to_owned(),
                detail: String::new(),
                created_unix_ms: unix_ms(),
                claim_token: None,
                cleanup: None,
                cleanup_detail: None,
            },
            None => {
                return Ok(RefusedLaunchCleanup::Pending(
                    "the failed launch has no valid durable refusal".to_owned(),
                ));
            }
        };
        refused_launch_cleanup(refused_directory, &refusal)
    })
}

fn release_reopen_marker_after_refusal_with(
    source_directory: &Path,
    refused_directory: &Path,
    outcome: Result<()>,
    cleanup: impl FnOnce() -> Result<RefusedLaunchCleanup>,
) -> Result<()> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let released = (|| -> Result<()> {
        let cleanup = cleanup()?;
        if let RefusedLaunchCleanup::Pending(reason) = &cleanup {
            let note = record_reopen_refusal_cleanup_pending(refused_directory, reason)
                .err()
                .map_or(String::new(), |error| {
                    format!("; the refusal record could not be annotated: {error:#}")
                });
            bail!("{cleanup}{note}");
        }
        let marker_path = Reader::open_unchecked(source_directory)
            .record(CoreRecord::ReopenMarker)
            .path()
            .to_owned();
        let _lock = Store::open_unchecked(source_directory).lock()?;
        let Some(text) = session::RecordReader::at(&marker_path).text()? else {
            return Ok(());
        };
        let marker: ReopenMarker = record::reopen_marker(&text).context("invalid reopen marker")?;
        if marker.reopened_by.as_deref() != Some(refused_session) {
            bail!(
                "reopen marker names {:?}, not the refused session {refused_session}",
                marker.reopened_by
            );
        }
        RecordStore::at(&marker_path).remove()
    })();
    let Err(release_error) = released else {
        return outcome;
    };
    let release_error = release_error.context(format!(
        "the reopen marker of source session {} was not released",
        source_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    ));
    match outcome {
        Ok(()) => Err(release_error),
        Err(error) => Err(error.context(format!("{release_error:#}"))),
    }
}

// Read-only: release eligibility requires a durable launch refusal and verified cleanup.
// Missing records or a possibly surviving provider keep the marker consumed after a crash.
pub(super) fn verify_reopen_source_is_closed(
    directory: &Path,
    id: &str,
) -> Result<Option<marker::StaleReopenMarker>> {
    let closed = Reader::open_unchecked(directory).closed_if_present()?;
    let status = Reader::open_unchecked(directory).status_if_present()?;
    let state = status
        .as_ref()
        .map_or("unknown", |status| status.state.as_str());
    if state != SessionState::Closed.as_str()
        || closed
            .as_ref()
            .is_none_or(|closed| closed.state != SessionState::Closed)
    {
        return Err(reopen_refusal(
            ReopenGate::SourceNotClosed,
            format!(
                "session {id} is {state}; reopen requires a session closed with its closed tombstone"
            ),
        ));
    }
    for name in [
        CoreRecord::TurnClaim.name(),
        CoreRecord::Completion.name(),
        CoreRecord::Terminal.name(),
        CoreRecord::TerminalClosing.name(),
    ] {
        if fs::symlink_metadata(Reader::open_unchecked(directory).private(name).path()).is_ok() {
            return Err(reopen_refusal(
                ReopenGate::SourceNotConverged,
                format!("session {id} still carries {name}; its close has not converged"),
            ));
        }
    }
    marker::observe(directory)?.into_stale(id)
}

// The winner's hold on a closed source. Dropping it before `finalize` removes the marker
// again, so a reopen that never created its session leaves the source reopenable.
#[derive(Debug)]
pub(super) struct Reservation {
    path: PathBuf,
    claim: String,
    finalized: bool,
}

impl Reservation {
    pub(super) fn finalize(mut self, new_session_id: &str) -> Result<()> {
        let _lock = Store::open_unchecked((self.path).with_file_name("")).lock()?;
        let text = session::RecordReader::at(&self.path)
            .text()?
            .context("reopen marker disappeared before the new session was recorded")?;
        let mut marker: ReopenMarker =
            record::reopen_marker(&text).context("invalid reopen marker")?;
        if marker.claim != self.claim {
            bail!("reopen marker belongs to a different reopen attempt");
        }
        marker.reopened_by = Some(new_session_id.to_owned());
        session::RecordStore::at(&self.path).write_json(&marker)?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let Ok(_lock) = Store::open_unchecked((self.path).with_file_name("")).lock() else {
            return;
        };
        let current = session::RecordReader::at(&self.path)
            .text()
            .ok()
            .flatten()
            .and_then(|text| record::reopen_marker(&text).ok());
        if current.is_some_and(|marker| marker.claim == self.claim) {
            let _ = RecordStore::at(&self.path).remove();
        }
    }
}

// Serializes concurrent reopens of one closed source under the source's own turn-claim lock:
// the closed gates are re-checked under the lock and the marker is created with
// `create_new`, so exactly one attempt can hold it.
impl Reservation {
    pub(super) fn claim(
        directory: &Path,
        id: &str,
        provider_session_id: &str,
    ) -> Result<Reservation> {
        let path = Reader::open_unchecked(directory)
            .record(CoreRecord::ReopenMarker)
            .path()
            .to_owned();
        let _lock = Store::open_unchecked(directory).lock()?;
        if let Some(stale) = verify_reopen_source_is_closed(directory, id)? {
            // The gate re-ran under the lock, so the stale marker still names a refused launch
            // whose cleanup is verified now; releasing it here is the reconciliation the
            // crashed or timed-out parent never performed.
            RecordStore::at(&path).remove().with_context(|| {
            format!(
                "could not release the stale reopen marker of session {id} (reopened as {}, refused at {}, {})",
                stale.refused_session, stale.gate, stale.cleanup
            )
        })?;
        }
        let claim = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            TURN_CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let marker = ReopenMarker {
            schema: 1,
            claim: claim.clone(),
            provider_session_id: provider_session_id.to_owned(),
            reopened_by: None,
            created_unix_ms: unix_ms(),
        };
        session::RecordStore::at(&path)
            .write_private(&serde_json::to_vec_pretty(&marker)?)
            .map_err(|error| {
                reopen_refusal(
                    ReopenGate::AlreadyReopened,
                    format!("could not claim session {id} for reopen: {error:#}"),
                )
            })?;
        Ok(Reservation {
            path,
            claim,
            finalized: false,
        })
    }
}

#[cfg(test)]
pub(in crate::native) mod tests;
