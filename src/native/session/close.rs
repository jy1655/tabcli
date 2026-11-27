//! Explicit close, interrupted-close convergence, and dead-owner repair.
use crate::native::session;
#[cfg(test)]
use crate::native::session::primitives::with_sync_log;
use crate::native::session::primitives::{EventsDirectory, unix_ms};
use crate::native::session::state::{
    ResidualSurface, SessionState, SessionStatus, update_status, update_status_with_residual,
};

use super::owner::{OwnerAttestation, OwnerRecord, attest_owner};
use crate::native::process_identity::ProcessObservation;
use crate::native::session::turn::{
    JournaledEventState, journaled_event_state, recover_pending_completion,
    recover_pending_completion_from_tombstone_locked, remove_turn_claim_locked,
};
use crate::native::session::{CoreRecord, Reader, RecordStore, Store, launch};
use crate::native::terminal;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::terminal::close_authority::owner_exit::core_retains_finished;
pub(in crate::native) use crate::native::terminal::close_authority::owner_exit::repair_owner_is_live;
use crate::native::terminal::close_authority::owner_exit::{RepairSurface, RepairSurfaces};
use crate::native::terminal::ownership::NativeSessionOwner;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::terminal::ownership::OwnerBinding;

use anyhow::{Context, Result, bail};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use serde::{Deserialize, Serialize};
use std::{
    fs::{self},
    path::Path,
    time::{Duration, Instant},
};

pub(in crate::native) fn prune_blocked_by(a: &OwnerAttestation) -> bool {
    !matches!(a.process, ProcessObservation::Gone(_))
}

/// The stored states in which dead-owner repair examines the owner record; in any other
/// state it leaves the session untouched.
pub(in crate::native) fn repair_examines_owner(state: &SessionState) -> bool {
    matches!(
        state,
        SessionState::Launching
            | SessionState::AwaitingInitialInput
            | SessionState::Running
            | SessionState::Ready
            | SessionState::Claimed
            | SessionState::ResumePending
            | SessionState::Working
            | SessionState::Exited
            | SessionState::Failed
    )
}

// The close is synchronous. Scope its record I/O and diagnostics as well as its
// terminal calls; ordinary callers of these shared primitives remain unchanged.
// This is thread-local, never inherited by another command/thread, and restored on
// every return or unwind. Nested repair may shorten but cannot renew the budget.
pub(in crate::native) mod budget {
    use super::*;
    pub(in crate::native) const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);
    pub(in crate::native) fn close_deadline() -> Instant {
        Instant::now() + CLOSE_TIMEOUT
    }
    pub(in crate::native) fn close_remaining(deadline: Instant) -> Result<Duration> {
        deadline.checked_duration_since(close_now()).filter(|remaining| !remaining.is_zero())
            .context("close deadline expired; closing was not confirmed and terminal-side work may have occurred; use --timeout-secs for a longer budget")
    }

    // The scope's clock when a test installed one, the system clock otherwise. Close
    // tests advance the installed clock at an action boundary without scheduler races.
    pub(in crate::native) fn close_now() -> Instant {
        CLOCK
            .with(|slot| slot.borrow_mut().as_mut().map(|clock| clock.now()))
            .unwrap_or_else(Instant::now)
    }

    // The scope's state: the deadline and the clock inherit independently. A guard
    // replaces one of them and restores only that one.
    thread_local! {
        static DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
        static CLOCK: std::cell::RefCell<Option<Box<dyn crate::native::clock::Clock>>> =
            const { std::cell::RefCell::new(None) };
    }
    enum Replaced {
        Deadline(Option<Instant>),
        #[cfg(test)]
        Clock(Option<Box<dyn crate::native::clock::Clock>>),
    }
    pub(in crate::native) struct Scope {
        previous: Replaced,
        _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    impl Scope {
        // Takes the minimum deadline and keeps the effective clock.
        pub(in crate::native) fn enter(deadline: Instant) -> Self {
            let previous = DEADLINE.with(|slot| {
                let previous = slot.get();
                slot.set(Some(previous.map_or(deadline, |outer| outer.min(deadline))));
                previous
            });
            Self {
                previous: Replaced::Deadline(previous),
                _same_thread: std::marker::PhantomData,
            }
        }
        // Replaces only the clock and keeps any deadline already in force. Obtain a
        // fake's shared handle before moving the clock in; the guard stays where the
        // scope is, and clones of the handle go into hooks.
        #[cfg(test)]
        pub(in crate::native) fn with_clock(
            clock: impl crate::native::clock::Clock + 'static,
        ) -> Self {
            let previous = CLOCK.with(|slot| slot.borrow_mut().replace(Box::new(clock)));
            Self {
                previous: Replaced::Clock(previous),
                _same_thread: std::marker::PhantomData,
            }
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            match &mut self.previous {
                Replaced::Deadline(previous) => DEADLINE.with(|slot| slot.set(*previous)),
                #[cfg(test)]
                Replaced::Clock(previous) => {
                    CLOCK.with(|slot| *slot.borrow_mut() = previous.take());
                }
            }
        }
    }
    pub(in crate::native) fn deadline() -> Option<Instant> {
        DEADLINE.with(std::cell::Cell::get)
    }
    pub(in crate::native) fn check() -> Result<()> {
        deadline()
            .map(crate::native::session::close::budget::close_remaining)
            .transpose()
            .map(|_| ())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Deserialize, Serialize)]
struct TerminalCloseIntent {
    managed_session_id: String,
    terminal: terminal::TerminalSession,
    owner: NativeSessionOwner,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_teardown_intent"
    )]
    teardown: Option<TeardownGroup>,
}

// Absence is the legacy format. A present field must deserialize as a complete
// payload: Option's usual null -> None behavior would grant legacy replay authority.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn deserialize_teardown_intent<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<TeardownGroup>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    TeardownGroup::deserialize(deserializer).map(Some)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Deserialize, Serialize)]
struct TeardownGroup {
    process_group: u32,
    reason: String,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) struct TerminalTeardownIntent {
    owner: NativeSessionOwner,
    pub process_group: u32,
    pub reason: String,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn record_terminal_close_intent(
    store: &Store,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
) -> Result<()> {
    store
        .record(CoreRecord::TerminalCloseIntent)
        .write_json(&TerminalCloseIntent {
            managed_session_id: expected_session_id.to_owned(),
            terminal: session.clone(),
            owner: owner.clone(),
            teardown: None,
        })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn record_terminal_teardown_intent(
    store: &Store,
    id: &str,
    surface: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
    process_group: u32,
    reason: &str,
) -> Result<()> {
    store
        .record(CoreRecord::TerminalCloseIntent)
        .write_json(&TerminalCloseIntent {
            managed_session_id: id.to_owned(),
            terminal: surface.clone(),
            owner: owner.clone(),
            teardown: Some(TeardownGroup {
                process_group,
                reason: reason.to_owned(),
            }),
        })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl TerminalTeardownIntent {
    pub(in crate::native) fn verify_owner(&self, observed: &NativeSessionOwner) -> Result<()> {
        if serde_json::to_value(observed)? != serde_json::to_value(&self.owner)? {
            bail!("terminal teardown intent does not match the attested owner and exact handle");
        }
        Ok(())
    }
}

// Validate the saved handle and teardown against the owner named in the intent.
// Herdr compares that owner with the one observation that selects live-owner close.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn terminal_teardown_intent(
    reader: &Reader,
    surface: &terminal::TerminalSession,
) -> Result<Option<TerminalTeardownIntent>> {
    let Some(text) = reader.record(CoreRecord::TerminalCloseIntent).text()? else {
        return Ok(None);
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => bail!("unreadable terminal close intent; the handle is retained"),
    };
    if value.get("teardown").is_none() {
        return Ok(None);
    }
    let intent: TerminalCloseIntent = serde_json::from_value(value)?;
    let current_owner = if core_retains_finished(surface.kind) {
        None
    } else {
        Some(reader.owner()?)
    };
    let owner = intent.owner;
    let teardown = intent.teardown.context("missing teardown intent")?;
    // A complete control record also needs both process groups, which the clauses comparing
    // them with the intent's group already require.
    if intent.terminal != *surface
        || surface.managed_session_id.as_deref() != Some(&intent.managed_session_id)
        || owner.binding(&intent.managed_session_id) != OwnerBinding::Bound
        || owner.process_group != Some(teardown.process_group)
        || owner.pid != teardown.process_group
        || teardown.process_group == 0
        || teardown.process_group > i32::MAX as u32
        || teardown.reason.is_empty()
        || owner.terminal_tty.as_deref().is_none_or(str::is_empty)
        || !owner.control_record().is_complete()
        || owner.terminal_process_group != Some(teardown.process_group)
    {
        bail!("terminal teardown intent does not match the attested owner and exact handle");
    }
    let replay = TerminalTeardownIntent {
        owner,
        process_group: teardown.process_group,
        reason: teardown.reason,
    };
    if let Some(current_owner) = current_owner {
        replay.verify_owner(&current_owner)?;
    }
    Ok(Some(replay))
}

// An explicit close can end the owner before the window is gone, so a
// close that fails after it can never verify that owner again, and the owner's death
// says nothing about the window. The intent that the close recorded first lets only
// that close finish: it names this managed session, this exact handle and this exact
// owner record, with the owner's whole identity. Anything else, unreadable or not,
// grants nothing.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn terminal_close_intent_owner(
    reader: &Reader,
    session: &terminal::TerminalSession,
    surface_outlives_owner: bool,
) -> Result<Option<NativeSessionOwner>> {
    if !surface_outlives_owner {
        return Ok(None);
    }
    let intent = reader.record(CoreRecord::TerminalCloseIntent).text()?;
    // A malformed owner grants nothing, as a malformed intent does; an unreadable one is the
    // read's error.
    let owner = match super::owner::observe(reader) {
        OwnerRecord::Recorded(recorded) => Some(recorded.owner),
        OwnerRecord::Unreadable(error) => return Err(error),
        OwnerRecord::Absent | OwnerRecord::Malformed { .. } => None,
    };
    let (Some(intent), Some(owner)) = (intent, owner) else {
        return Ok(None);
    };
    let Ok(intent) = serde_json::from_str::<TerminalCloseIntent>(&intent) else {
        return Ok(None);
    };
    let attested = owner.control_record().is_complete();
    let bound = session.managed_session_id.as_deref() == Some(intent.managed_session_id.as_str())
        && owner.binding(&intent.managed_session_id) == OwnerBinding::Bound;
    let exact = intent.terminal == *session
        && serde_json::to_value(&intent.owner)? == serde_json::to_value(&owner)?;
    Ok((attested && bound && exact && intent.teardown.is_none()).then_some(owner))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn terminal_close_resumable(
    reader: &Reader,
    session: &terminal::TerminalSession,
    surface_outlives_owner: bool,
) -> Result<bool> {
    terminal_close_resumable_with(reader, session, surface_outlives_owner, attest_owner)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) fn terminal_close_resumable_with(
    reader: &Reader,
    session: &terminal::TerminalSession,
    surface_outlives_owner: bool,
    observe: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
) -> Result<bool> {
    Ok(
        terminal_close_intent_owner(reader, session, surface_outlives_owner)?
            .is_some_and(|owner| matches!(observe(&owner).process, ProcessObservation::Gone(_))),
    )
}

impl Reader {
    pub(in crate::native) fn has_active_session_capability(&self) -> bool {
        // Any entry counts, a link or a directory included: none of them is proof that the
        // record is gone.
        [
            CoreRecord::Terminal,
            CoreRecord::TerminalClosing,
            CoreRecord::TurnClaim,
            CoreRecord::Completion,
            CoreRecord::LegacyResumePending,
            CoreRecord::LegacyResumeRunning,
        ]
        .into_iter()
        .any(|record| fs::symlink_metadata(self.record(record).path()).is_ok())
    }
}

impl Reader {
    /// Prune attests the recorded process whatever Session the record names; a record that
    /// cannot be read or decoded blocks it.
    pub(in crate::native) fn native_owner_blocks_prune(&self) -> Result<bool> {
        Ok(match super::owner::observe(self) {
            OwnerRecord::Absent => false,
            OwnerRecord::Unreadable(_) | OwnerRecord::Malformed { .. } => true,
            OwnerRecord::Recorded(recorded) => prune_blocked_by(&attest_owner(&recorded.owner)),
        })
    }
}

// Repairs a dead native owner first, then closes. A repair failure is the recorded close
// error; otherwise `reason` (if any) is kept in the closed status.
pub(in crate::native) fn close(
    store: &Store,
    reason: Option<String>,
    deadline: Instant,
    close_terminal: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
) -> Result<terminal::CloseOutcome> {
    close_with_attestation(store, reason, deadline, attest_owner, close_terminal)
}

// Runs the same close procedure, with only the owner's OS observation supplied by
// the caller. Recovery, repair policy, surface retention and close ordering stay fixed.
pub(in crate::native) fn close_with_attestation(
    store: &Store,
    reason: Option<String>,
    deadline: Instant,
    observe: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
    closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
) -> Result<terminal::CloseOutcome> {
    close_transaction(
        store,
        reason,
        |marker| repair_attested_locked(store, observe, deadline, marker),
        closer,
        deadline,
    )
    .map_err(
        |error| match crate::native::session::close::budget::close_remaining(deadline) {
            Ok(_) => error,
            // The expiry itself already says so; add it only to an error that does not.
            Err(expired)
                if error
                    .chain()
                    .any(|cause| cause.to_string() == expired.to_string()) =>
            {
                error
            }
            Err(expired) => error.context(expired.to_string()),
        },
    )
}

#[cfg(test)]
fn close_with_repair(
    store: &Store,
    reason: Option<String>,
    repair: impl FnOnce() -> Result<bool>,
    closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
    deadline: Instant,
) -> Result<terminal::CloseOutcome> {
    close_transaction(store, reason, |_| repair(), closer, deadline)
}

fn close_transaction(
    store: &Store,
    reason: Option<String>,
    repair: impl FnOnce(&mut MarkerHistory) -> Result<bool>,
    mut close_terminal: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    let _budget = budget::Scope::enter(deadline);
    crate::native::session::close::budget::close_remaining(deadline)?;
    let _lock = store.lock().map_err(|error| match budget::check() {
        // Keep the lock-timeout diagnosis previously supplied by pre-close recovery.
        Err(expired) => {
            anyhow::anyhow!("pre-close session repair failed: {error:#}").context(expired)
        }
        Ok(()) => error,
    })?;
    let mut marker = MarkerHistory::default();
    let repair_error = match repair(&mut marker) {
        Err(error) if error.is::<CloseRecordChanged>() => return Err(error),
        Err(error) => Some(format!("pre-close session repair failed: {error:#}")),
        Ok(_) => None,
    };
    if let Err(expired) = budget::check() {
        return Err(match repair_error {
            Some(error) => anyhow::anyhow!(error).context(expired),
            None => expired,
        });
    }
    let mut outcome = terminal::CloseOutcome::Missing;
    close_session_state_locked(
        store,
        &mut marker,
        repair_error.or(reason),
        |surface| {
            let result = close_terminal(surface)?;
            outcome = result.outcome;
            Ok(result)
        },
        deadline,
    )?;
    Ok(outcome)
}

fn close_session_state_with_error<F>(
    store: &Store,
    close_error: Option<String>,
    close_terminal: F,
    deadline: Instant,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
{
    let _budget = budget::Scope::enter(deadline);
    budget::close_remaining(deadline)?;
    let _lock = store.lock()?;
    close_session_state_locked(
        store,
        &mut MarkerHistory::default(),
        close_error,
        close_terminal,
        deadline,
    )
}

fn close_session_state_locked<F>(
    store: &Store,
    marker: &mut MarkerHistory,
    mut close_error: Option<String>,
    mut close_terminal: F,
    deadline: Instant,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
{
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let records = CloseRecords::acquire(store, marker)?;
    let status = records.status?;
    if status.state == SessionState::Closed {
        let consume_result =
            consume_terminal_handle(store, marker, records.marker, None, close_error.as_deref());
        return finish_consumption(consume_result, || {
            mark_session_closed_locked(store, marker, &claim_path, close_error.or(status.error))
        });
    }

    let terminal_path = store.record(CoreRecord::Terminal).path().to_owned();
    let closing = store.record(CoreRecord::TerminalClosing);
    let closing_path = closing.path().to_owned();
    if let Some(facts) = records.marker? {
        close_error = merge_close_reason(close_error, facts.reason.clone());
        let consume_result =
            consume_terminal_handle(store, marker, Ok(Some(facts)), None, close_error.as_deref());
        return finish_consumption(consume_result, || {
            mark_session_closed_locked(store, marker, &claim_path, close_error)
        });
    }
    // No ownership gate sees a handle-less session. Fence this close while the
    // pending launcher can still return a surface, under the same lifecycle lock.
    if status.state == SessionState::Launching
        && !records.active.present(store)?
        && !records.closing.present(store)?
        && records.closed?.is_none()
        && let Some(record) = launch::read(store)?
        && record.phase == launch::Phase::Pending
        && record.deadline_unix_ms > unix_ms()
    {
        bail!(
            "the launcher is still creating the surface for this session (launch deadline {}); no handle exists yet and nothing was closed; close again after the deadline or once the session has failed",
            record.deadline_unix_ms
        );
    }
    let active = records.active.decode(store);
    let claimed = records.closing.decode(store);
    let renamed = match store.claim_terminal_handle()? {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim terminal handle {}",
                    terminal_path.display()
                )
            });
        }
    };
    let after_claim = CloseRecords::acquire(store, marker)?;
    if !renamed && !after_claim.closing.present(store)? {
        // No adapter will run for this residual surface. Keep its warning in
        // the new tombstone; ordinary handle-less failures keep prior behavior.
        let residual = status.residual_surface().and(status.error);
        let close_error = match residual {
            Some(residual) => Some(match close_error {
                Some(error) => format!("{error}; {residual}"),
                None => residual,
            }),
            None => close_error,
        };
        return mark_session_closed_locked(store, marker, &claim_path, close_error);
    }
    // Resume an existing claim only when the acquisition found no active handle.
    let selected = if !renamed && matches!(active, Ok(None)) {
        claimed
    } else {
        active
    };
    let verified = selected.and_then(|handle| {
        let handle = handle.ok_or(CloseRecordChanged("terminal handle"))?;
        let bytes = after_claim
            .closing
            .bytes
            .context(CloseRecordChanged("claimed terminal handle"))?;
        if bytes.as_deref() != Some(handle.bytes.as_slice()) {
            return Err(CloseRecordChanged("terminal handle").into());
        }
        handle.decoded
    });
    let (terminal, stated_kind) = match verified {
        Ok(handle) => handle,
        Err(error) => {
            if let Err(restore) = restore_terminal_handle(&closing_path, &terminal_path) {
                return if error.is::<CloseRecordChanged>() {
                    Err(restore.context(CloseRecordChanged("terminal close records")))
                } else {
                    Err(restore)
                };
            }
            return Err(error);
        }
    };
    let result = match crate::native::session::close::budget::close_remaining(deadline)
        .and_then(|_| close_terminal(&terminal))
        .and_then(|result| {
            crate::native::session::close::budget::close_remaining(deadline)?;
            Ok(result)
        }) {
        Ok(result) => result,
        Err(error) => {
            restore_terminal_handle(&closing_path, &terminal_path)?;
            return Err(error);
        }
    };
    if result.reason.is_some() || result.keeps_diagnosis {
        let diagnosis = if result.keeps_diagnosis {
            status.error.clone().or(result.generated_reason)
        } else {
            status.error.clone()
        };
        close_error = merge_close_reason(merge_close_reason(close_error, diagnosis), result.reason);
    }

    let after_adapter = CloseRecords::acquire(store, marker)?;
    let consume_result = consume_terminal_handle(
        store,
        marker,
        after_adapter.marker,
        stated_kind.map(terminal::TerminalKind::as_str),
        close_error.as_deref(),
    );
    // Only this path has the adapter's positive close/absence result. No-handle
    // close, repair, and replay of a consumed handle cannot clear the observation.
    let cleared = status.residual_surface().map(|_| ResidualSurface::Cleared);
    finish_consumption(consume_result, || {
        mark_session_closed_with_residual_locked(store, marker, &claim_path, close_error, cleared)
    })
}

// Native consumption can fail after changing records. Finish from disk and return the
// original error, unless either step observed different bytes and requires a new attempt.
fn finish_consumption(consumption: Result<()>, finish: impl FnOnce() -> Result<()>) -> Result<()> {
    if consumption
        .as_ref()
        .is_err_and(|error| error.is::<CloseRecordChanged>())
    {
        return consumption;
    }
    match finish() {
        Err(error) if error.is::<CloseRecordChanged>() => Err(error),
        result => consumption.and(result),
    }
}

fn restore_terminal_handle(closing_path: &Path, terminal_path: &Path) -> Result<()> {
    RecordStore::at(closing_path)
        .rename_to(&RecordStore::at(terminal_path))
        .with_context(|| {
            format!(
                "failed to restore terminal handle {} after close failure",
                terminal_path.display()
            )
        })
}

#[derive(Debug)]
struct CloseRecordChanged(&'static str);

impl std::fmt::Display for CloseRecordChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed during close. Retry to use the current records",
            self.0
        )
    }
}

impl std::error::Error for CloseRecordChanged {}

struct HandleSlot {
    name: CoreRecord,
    bytes: Result<Option<Vec<u8>>>,
}

struct AcquiredHandle {
    bytes: Vec<u8>,
    decoded: Result<(terminal::TerminalSession, Option<terminal::TerminalKind>)>,
}

impl HandleSlot {
    fn read(store: &Store, name: CoreRecord) -> Self {
        Self {
            name,
            bytes: store.record(name).bytes(),
        }
    }

    fn present(&self, store: &Store) -> Result<bool> {
        match &self.bytes {
            Ok(value) => Ok(value.is_some()),
            Err(_) => store.record(self.name).present(),
        }
    }

    fn decode(self, store: &Store) -> Result<Option<AcquiredHandle>> {
        let record = store.record(self.name);
        Ok(self.bytes?.map(|bytes| {
            let decoded = std::str::from_utf8(&bytes)
                .map_err(|_| super::primitives::invalid_record_text(record.path()))
                .and_then(|text| {
                    terminal::TerminalSession::decode_with_stated_kind(text)
                        .with_context(|| format!("invalid JSON in {}", record.path().display()))
                });
            AcquiredHandle { bytes, decoded }
        }))
    }
}

#[derive(Clone, Default)]
struct MarkerFacts {
    committed: bool,
    reason: Option<String>,
}

impl MarkerFacts {
    fn decode(bytes: &[u8]) -> Self {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| super::record::consumption_marker(text).ok())
            .map(|marker| Self {
                committed: marker.committed(),
                reason: marker.reason().map(str::to_owned),
            })
            .unwrap_or_default()
    }
}

#[derive(Default)]
enum OriginalMarker {
    #[default]
    Unobserved,
    Unreadable,
    Acquired {
        bytes: Option<Vec<u8>>,
        facts: Option<MarkerFacts>,
    },
}

#[derive(Default)]
struct MarkerHistory {
    original: OriginalMarker,
    written: Option<(Vec<u8>, MarkerFacts)>,
}

impl MarkerHistory {
    fn observe(&mut self, store: &Store) -> Result<Option<&MarkerFacts>> {
        let bytes = store
            .record(CoreRecord::TerminalClosed)
            .bytes()
            .inspect_err(|_| {
                if matches!(self.original, OriginalMarker::Unobserved) {
                    self.original = OriginalMarker::Unreadable;
                }
            })?;
        if let Some((written, facts)) = &self.written
            && bytes.as_ref() == Some(written)
        {
            return Ok(Some(facts));
        }
        if matches!(self.original, OriginalMarker::Unobserved) {
            let facts = bytes.as_deref().map(MarkerFacts::decode);
            self.original = OriginalMarker::Acquired { bytes, facts };
        } else if !matches!(&self.original, OriginalMarker::Acquired { bytes: original, .. } if original == &bytes)
        {
            return Err(CloseRecordChanged("terminal consumption marker").into());
        }
        match &self.original {
            OriginalMarker::Acquired { facts, .. } => Ok(facts.as_ref()),
            _ => unreachable!("a successful observation acquired the original marker"),
        }
    }

    fn check_for_change(&mut self, store: &Store) -> Result<()> {
        match self.observe(store) {
            Err(error) if error.is::<CloseRecordChanged>() => Err(error),
            _ => Ok(()),
        }
    }
}

struct CloseRecords {
    status: Result<SessionStatus>,
    closed: Result<Option<SessionStatus>>,
    marker: Result<Option<MarkerFacts>>,
    active: HandleSlot,
    closing: HandleSlot,
}

impl CloseRecords {
    fn acquire(store: &Store, history: &mut MarkerHistory) -> Result<Self> {
        let marker = history.observe(store).map(|facts| facts.cloned());
        match marker {
            Err(error) if error.is::<CloseRecordChanged>() => Err(error),
            marker => Ok(Self {
                status: store.status(),
                closed: store.closed_if_present(),
                marker,
                active: HandleSlot::read(store, CoreRecord::Terminal),
                closing: HandleSlot::read(store, CoreRecord::TerminalClosing),
            }),
        }
    }
}

#[cfg(test)]
fn terminal_consumption_reason(store: &Store) -> Result<Option<String>> {
    Ok(MarkerHistory::default()
        .observe(store)?
        .and_then(|facts| facts.reason.clone()))
}

/// Joins two close reasons with `; `. A reason that the other already carries, whole or as a
/// run of its `; `-joined parts, is not added again: a replay of a committed consumption marker
/// meets the reason it committed, and the retry's own reason is already part of it.
fn merge_close_reason(first: Option<String>, second: Option<String>) -> Option<String> {
    fn carries(whole: &str, part: &str) -> bool {
        whole == part
            || whole.starts_with(&format!("{part}; "))
            || whole.ends_with(&format!("; {part}"))
            || whole.contains(&format!("; {part}; "))
    }
    match (first, second) {
        (Some(first), Some(second)) if carries(&first, &second) => Some(first),
        (Some(first), Some(second)) if carries(&second, &first) => Some(second),
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (first, second) => first.or(second),
    }
}

#[cfg(test)]
fn committed_consumption_marker(store: &Store) -> Result<bool> {
    Ok(MarkerHistory::default()
        .observe(store)?
        .is_some_and(|facts| facts.committed))
}

fn consume_terminal_handle(
    store: &Store,
    history: &mut MarkerHistory,
    facts: Result<Option<MarkerFacts>>,
    terminal_kind: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    // A committed marker (one that decodes with `consumed: true`) keeps its bytes: a replay, and
    // a later close of the closed session, neither rewrites its reason nor its kind. A damaged
    // one (not a marker, or `consumed: false`) is written again, as before os-parity.
    let tombstone_result = facts.and_then(|facts| {
        if facts.is_some_and(|marker| marker.committed) {
            return Ok(());
        }
        let mut marker = serde_json::json!({
            "consumed": true,
            "terminal": terminal_kind,
        });
        if let Some(reason) = reason {
            marker["reason"] = reason.into();
        }
        history.written = Some((
            serde_json::to_vec_pretty(&marker)?,
            MarkerFacts {
                committed: true,
                reason: reason.map(str::to_owned),
            },
        ));
        store.write_terminal_closed(&marker)
    });
    #[cfg(test)]
    if tombstone_result.is_ok() {
        interruption::at(interruption::Point::ConsumptionMarkerWritten);
    }
    // A consumption marker is durable authority for the next close. After it is
    // written, expiry leaves the claim in place; this is never a handle rollback.
    budget::check()?;
    // A failed sync may have published the intended bytes. A different writer's
    // marker ends this attempt before the remaining handles are removed.
    history.check_for_change(store)?;
    let active_result = RecordStore::at(store.record(CoreRecord::Terminal).path()).remove();
    let closing_result = RecordStore::at(store.record(CoreRecord::TerminalClosing).path()).remove();
    #[cfg(test)]
    if active_result.is_ok() && closing_result.is_ok() {
        interruption::at(interruption::Point::HandleRemoved);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let intent_result =
        RecordStore::at(store.record(CoreRecord::TerminalCloseIntent).path()).remove();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let intent_result = Ok(());
    tombstone_result?;
    active_result?;
    closing_result?;
    intent_result
}

// Finishes a close whose tombstone was written but whose later cleanup steps did not run.
// The tombstone is preserved unchanged: status.json is rewritten from it (update_status
// copies the tombstone whenever one exists), and the turn claim, the journal, and the
// legacy resume markers are removed in the same order the uninterrupted close uses. A
// journaled completion is settled exactly as that close settles it: an event it already
// wrote stays published when it matches the journal, is set aside when it does not, and a
// journal without an event is discarded.
pub(in crate::native) fn converge_interrupted(
    store: &Store,
    claim_path: &Path,
    tombstone: &SessionStatus,
) -> Result<bool> {
    let directory = store.directory();
    let mut changed = false;
    let status_matches = store.status_if_present()?.is_some_and(|status| {
        status.state == tombstone.state
            && status.generation == tombstone.generation
            && status.error == tombstone.error
            && status.residual_surface == tombstone.residual_surface
    });
    if !status_matches {
        update_status(
            directory,
            SessionState::Closed,
            None,
            tombstone.error.clone(),
        )?;
        changed = true;
    }
    let completion = store.record(CoreRecord::Completion);
    if completion.present()? {
        set_aside_unverified_completion_event_for_close(store)?;
        changed = true;
    }
    if session::CoreRecordReader::at(claim_path).present()? {
        remove_turn_claim_locked(claim_path)?;
        changed = true;
    }
    completion.remove()?;
    for record in [
        CoreRecord::LegacyResumePending,
        CoreRecord::LegacyResumeRunning,
    ] {
        let record = store.record(record);
        if record.present()? {
            record.remove()?;
            changed = true;
        }
    }
    Ok(changed)
}

/// The first settlement step of a close that finds a completion journal in place. The
/// tombstone is the close's commit point, but an event the interrupted completion already
/// wrote is the provider's authoritative result: when it matches the journal byte for byte
/// it stays published (the receipt already maps the request to it), and when it does not
/// match, or is too large to compare, it is moved aside under an `unpublished-` name that
/// no query reads. A journal whose event was never written needs no step here and is
/// discarded when the close removes the journal; a session whose `events` directory is
/// missing altogether holds no event to verify and settles the same way, so a close is
/// never left permanently unsettled by that damage. A link or a non-directory at `events`
/// is still rejected, and the journal then stays in place with the claim. The move is
/// idempotent, so an interrupted close converges on the next run.
///
/// The close removes the journal only after this step and after the turn claim is
/// released: while the claim is installed, the journal is the evidence that the event at
/// its path is the committed result, so an interruption before claim release would
/// otherwise hide a published result until the next recovery. The journal is also the
/// only evidence that a set-aside event was unverified, so the move is made durable
/// before the journal can be discarded: the rename syncs `events/` itself, and a run that
/// finds the event already moved aside by an interrupted close, which may have stopped
/// between the rename and that sync, syncs `events/` again before it returns. A committed
/// event gets the same barrier: the completion that wrote it may have stopped between its
/// rename and the sync of `events/`, so the close syncs the directory before the journal,
/// the only proof that the entry is the result, is removed.
fn set_aside_unverified_completion_event_for_close(store: &Store) -> Result<()> {
    let directory = store.directory();
    let Some(bytes) = store.record(CoreRecord::Completion).bytes()? else {
        return Ok(());
    };
    if store.events_directory_state()? == EventsDirectory::Missing {
        return Ok(());
    }
    // A journal that does not decode is not evidence; the close settles without it.
    let Ok(pending) = super::record::journal(&bytes) else {
        return Ok(());
    };
    let events = store.record(CoreRecord::Events).path().to_owned();
    let set_aside = store
        .unpublished_event(&pending.event_file)
        .path()
        .to_owned();
    match journaled_event_state(directory, &pending)? {
        JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => {
            RecordStore::at(&events.join(&pending.event_file))
                .rename_to(&RecordStore::at(&set_aside))
                .context(
                    "failed to set aside a completion event that disagrees with its journal",
                )?;
        }
        JournaledEventState::Absent => {
            if fs::symlink_metadata(&set_aside).is_ok() {
                store.sync_set_aside_event_directory()?;
            }
        }
        JournaledEventState::Committed => store.sync_committed_event_directory()?,
    }
    Ok(())
}

#[cfg(test)]
fn mark_session_closed(store: &Store, error: Option<String>) -> Result<()> {
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let _lock = store.lock()?;
    mark_session_closed_locked(store, &mut MarkerHistory::default(), &claim_path, error)
}

fn mark_session_closed_locked(
    store: &Store,
    marker: &mut MarkerHistory,
    claim_path: &Path,
    error: Option<String>,
) -> Result<()> {
    mark_session_closed_with_residual_locked(store, marker, claim_path, error, None)
}

fn mark_session_closed_with_residual_locked(
    store: &Store,
    marker: &mut MarkerHistory,
    claim_path: &Path,
    error: Option<String>,
    residual_surface: Option<ResidualSurface>,
) -> Result<()> {
    let directory = store.directory();
    let records = CloseRecords::acquire(store, marker)?;
    let facts = records.marker?;
    let error = merge_close_reason(error, facts.as_ref().and_then(|facts| facts.reason.clone()));
    let consume_result = (|| {
        if records.active.present(store)?
            || records.closing.present(store)?
            || facts.as_ref().is_some_and(|facts| !facts.committed)
        {
            consume_terminal_handle(store, marker, Ok(facts), None, error.as_deref())
        } else {
            Ok(())
        }
    })();
    if consume_result
        .as_ref()
        .is_err_and(|error| error.is::<CloseRecordChanged>())
    {
        return consume_result;
    }
    marker.check_for_change(store)?;
    let status_result = update_status_with_residual(
        directory,
        SessionState::Closed,
        None,
        error,
        residual_surface,
    );
    marker.check_for_change(store)?;
    let (pending_result, running_result, claim_result) = if status_result.is_ok() {
        // An event the journal disagrees with is set aside first, then the claim is
        // released, and only then is the journal removed: every interruption of this order
        // leaves a state in which a committed event stays published and an unverified one
        // stays hidden. The journal is kept whenever an earlier step failed.
        let claim_result = set_aside_unverified_completion_event_for_close(store)
            .and_then(|()| remove_turn_claim_locked(claim_path));
        let pending_result = if claim_result.is_ok() {
            RecordStore::at(store.record(CoreRecord::Completion).path()).remove()
        } else {
            Ok(())
        }
        .and_then(|()| {
            RecordStore::at(store.record(CoreRecord::LegacyResumePending).path()).remove()
        });
        (
            pending_result,
            RecordStore::at(store.record(CoreRecord::LegacyResumeRunning).path()).remove(),
            claim_result,
        )
    } else {
        (Ok(()), Ok(()), Ok(()))
    };
    consume_result?;
    status_result?;
    pending_result?;
    running_result?;
    claim_result
}

pub(in crate::native) fn repair_dead_owner(store: &Store) -> Result<bool> {
    repair_dead_owner_until(
        store,
        crate::native::session::close::budget::close_deadline(),
    )
}

pub(in crate::native) fn repair_dead_owner_until(store: &Store, deadline: Instant) -> Result<bool> {
    repair_dead_owner_attested_until(store, attest_owner, deadline)
}

/// Dead-owner Repair with the owner's OS observation supplied by the caller, under this
/// platform's surface policy.
pub(in crate::native) fn repair_dead_owner_attested_until(
    store: &Store,
    attest: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
    deadline: Instant,
) -> Result<bool> {
    #[cfg(windows)]
    return repair_dead_owner_windows_with(store, attest, terminal::windows_installed(), deadline);

    #[cfg(not(windows))]
    {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let surfaces = RepairSurfaces::Posix;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let surfaces = RepairSurfaces::Unsupported;
        repair_dead_owner_with(
            store,
            |owner| repair_owner_is_live(&attest(owner)),
            surfaces,
            deadline,
        )
    }
}

fn repair_attested_locked(
    store: &Store,
    attest: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
    deadline: Instant,
    marker: &mut MarkerHistory,
) -> Result<bool> {
    #[cfg(windows)]
    let mut close = |surface: &terminal::TerminalSession| {
        terminal::windows_dispatch::close_session_until(
            terminal::windows_installed(),
            store.directory(),
            surface,
            deadline,
        )
    };
    #[cfg(windows)]
    let surfaces = RepairSurfaces::Windows(&mut close);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let surfaces = RepairSurfaces::Posix;
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    let surfaces = RepairSurfaces::Unsupported;
    repair_owner(
        store,
        |owner| repair_owner_is_live(&attest(owner)),
        surfaces,
        deadline,
        Some(marker),
    )
}

/// Repair parity for fixtures that record no surface. POSIX close intents are retained,
/// and a surface at settlement is an error.
#[cfg(test)]
pub(in crate::native) fn repair_dead_owner_attested_with(
    store: &Store,
    attest: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
) -> Result<bool> {
    repair_dead_owner_with(
        store,
        |owner| repair_owner_is_live(&attest(owner)),
        RepairSurfaces::WithoutSurface,
        budget::close_deadline(),
    )
}

#[cfg(any(windows, all(test, any(target_os = "macos", target_os = "linux"))))]
pub(in crate::native) fn repair_dead_owner_windows_with(
    store: &Store,
    attest: impl FnOnce(&NativeSessionOwner) -> OwnerAttestation,
    adapters: &dyn terminal::windows_dispatch::Adapters,
    deadline: Instant,
) -> Result<bool> {
    let mut close = |surface: &terminal::TerminalSession| {
        terminal::windows_dispatch::close_session_until(
            adapters,
            store.directory(),
            surface,
            deadline,
        )
    };
    repair_dead_owner_with(
        store,
        |owner| repair_owner_is_live(&attest(owner)),
        RepairSurfaces::Windows(&mut close),
        deadline,
    )
}

fn repair_dead_owner_with(
    store: &Store,
    owner_is_live: impl FnOnce(&NativeSessionOwner) -> Result<bool>,
    surfaces: RepairSurfaces<'_>,
    deadline: Instant,
) -> Result<bool> {
    repair_owner(store, owner_is_live, surfaces, deadline, None)
}

// A marker history belongs to the caller holding the lifecycle lock. Standalone
// Repair uses its usual recovery lock, launch try-lock, and settlement lock.
fn repair_owner(
    store: &Store,
    owner_is_live: impl FnOnce(&NativeSessionOwner) -> Result<bool>,
    surfaces: RepairSurfaces<'_>,
    deadline: Instant,
    mut locked: Option<&mut MarkerHistory>,
) -> Result<bool> {
    let directory = store.directory();
    // Completion recovery runs first so a live owner's finished turn is published before
    // anything else is decided. Its failure is damage (a missing `events/`, a journal
    // whose event cannot be compared), not a reason to leave a dead owner's session
    // installed forever: the owner check still runs, a dead owner's session is closed as
    // it would be without the damage (the close settles the journal without publishing),
    // and the damage is reported in the close error. Under a live owner, or when the
    // owner cannot be shown dead, the damage is the result.
    let claim = store.record(CoreRecord::TurnClaim);
    let recovery_damage = if let Some(marker) = locked.as_deref_mut() {
        CloseRecords::acquire(store, marker)?
            .closed
            .and_then(|tombstone| {
                recover_pending_completion_from_tombstone_locked(directory, claim.path(), tombstone)
            })
    } else {
        recover_pending_completion(directory)
    }
    .err();
    // A bounded wait is not journal damage and grants no repair authority.
    if let Err(expired) = budget::check() {
        return Err(recovery_damage.unwrap_or(expired));
    }
    let untouched = |damage: Option<anyhow::Error>| match damage {
        Some(error) => Err(error),
        None => Ok(false),
    };
    let launch_pending = if let Some(marker) = locked.as_deref_mut() {
        // Recovery can have published an event or restored status before a later error.
        let records = CloseRecords::acquire(store, marker)?;
        launch::repair_locked(store, records.status)?
    } else {
        launch::repair(store)?
    };
    if launch_pending {
        return untouched(recovery_damage);
    }
    let status = match locked.as_deref_mut() {
        Some(marker) => CloseRecords::acquire(store, marker)?.status?,
        None => store.status()?,
    };
    if !repair_examines_owner(&status.state) {
        return untouched(recovery_damage);
    }
    // Repair attests the recorded process whatever Session the record names.
    let Some(owner) = super::owner::observe(store).decoded()? else {
        return untouched(recovery_damage);
    };
    let owner = owner.owner;
    let owner_live = owner_is_live(&owner)?;
    if owner_live {
        return untouched(recovery_damage);
    }
    let mut repair_error = status
        .error
        .clone()
        .unwrap_or_else(|| format!("native session process {} is no longer running", owner.pid));
    if let Some(damage) = &recovery_damage {
        repair_error = format!("{repair_error}; completion recovery failed: {damage:#}");
    }
    let repair_error = Some(repair_error);
    match surfaces.decide(store, &status.state, deadline)? {
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        RepairSurface::Retain => return untouched(recovery_damage),
        RepairSurface::Settle(retention) => {
            // Standalone Repair scans before taking the lock. Check retention under
            // the lock in case a closer restored a handle after that scan.
            let _lock = if locked.is_none() {
                Some(store.lock()?)
            } else {
                None
            };
            let mut fresh = MarkerHistory::default();
            let marker = locked.unwrap_or(&mut fresh);
            if !repair_examines_owner(&store.status()?.state) || retention.retains(store)? {
                return untouched(recovery_damage);
            }
            mark_session_closed_locked(store, marker, claim.path(), repair_error)?;
        }
        RepairSurface::CloseOnWindows(mut close) => {
            let context = close.failure_context();
            let closer = |surface: &terminal::TerminalSession| {
                close.close(surface, deadline).map(Into::into)
            };
            match locked {
                Some(marker) => {
                    close_session_state_locked(store, marker, repair_error, closer, deadline)
                }
                None => close_session_state_with_error(store, repair_error, closer, deadline),
            }
            .context(context)?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod owner_read_tests;
#[cfg(test)]
mod read_policy_tests;
#[cfg(test)]
mod tests;

// A panic models interruption, not an operation error: no caller's restore/settle
// handler runs. File locks unwind; only the selected thread's one checkpoint fires.
// Every OS interrupts at the consumption marker and the handle removal; the other points and
// the expiry checkpoint serve macOS scenarios.
#[cfg(test)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(in crate::native) mod interruption {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Instant,
    };
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(in crate::native) enum Point {
        IntentPublished,
        SignalSent,
        ConsumptionMarkerWritten,
        HandleRemoved,
        ClosedTombstoneWritten,
    }
    // The expiry checkpoint writes the deadline into the scope's fake clock through
    // the handle the test obtained before installing that clock.
    struct Expiry {
        at: Point,
        deadline: Instant,
        time: Rc<Cell<Instant>>,
    }
    thread_local! {
        static EXPIRE: RefCell<Option<Expiry>> = const { RefCell::new(None) };
        static STOP: Cell<Option<Point>> = const { Cell::new(None) };
    }
    pub(in crate::native) fn at(point: Point) {
        EXPIRE.with(|slot| {
            if let Some(expiry) = slot.borrow().as_ref()
                && expiry.at == point
            {
                expiry.time.set(expiry.deadline);
            }
        });
        if STOP.with(|stop| stop.get() == Some(point)) {
            STOP.with(|stop| stop.set(None));
            std::panic::panic_any(point);
        }
    }
    pub(super) fn expiring<T>(
        point: Point,
        deadline: Instant,
        time: Rc<Cell<Instant>>,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                EXPIRE.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        EXPIRE.with(|slot| {
            *slot.borrow_mut() = Some(Expiry {
                at: point,
                deadline,
                time,
            })
        });
        run()
    }
    pub(in crate::native) fn during<T>(
        point: Point,
        run: impl FnOnce() -> T,
    ) -> std::thread::Result<T> {
        struct Reset(Option<Point>);
        impl Drop for Reset {
            fn drop(&mut self) {
                STOP.with(|stop| stop.set(self.0));
            }
        }
        let _reset = Reset(STOP.with(|stop| stop.replace(Some(point))));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
    }
}
