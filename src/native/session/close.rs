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
    remove_turn_claim_locked,
};
use crate::native::session::{CoreRecord, Reader, RecordStore, Store, launch};
use crate::native::terminal;
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
    teardown: Option<TerminalTeardownIntent>,
}

// Absence is the legacy format. A present field must deserialize as a complete
// payload: Option's usual null -> None behavior would grant legacy replay authority.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn deserialize_teardown_intent<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<TerminalTeardownIntent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    TerminalTeardownIntent::deserialize(deserializer).map(Some)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Deserialize, Serialize)]
pub(in crate::native) struct TerminalTeardownIntent {
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
            teardown: Some(TerminalTeardownIntent {
                process_group,
                reason: reason.to_owned(),
            }),
        })
}

// Teardown is independent of surface retention policy, including iTerm2. A record
// claiming this intent kind must validate; it may never fall through to dead-owner
// absence based on the surface alone.
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
    let owner: NativeSessionOwner = reader.owner()?;
    let teardown = intent.teardown.context("missing teardown intent")?;
    // A complete control record also needs both process groups, which the clauses comparing
    // them with the intent's group already require.
    if intent.terminal != *surface
        || surface.managed_session_id.as_deref() != Some(&intent.managed_session_id)
        || owner.binding(&intent.managed_session_id) != OwnerBinding::Bound
        || serde_json::to_value(&owner)? != serde_json::to_value(&intent.owner)?
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
    Ok(Some(teardown))
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
    close_with_repair(
        store,
        reason,
        || repair_dead_owner_attested_until(store, observe, deadline),
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

fn close_with_repair(
    store: &Store,
    reason: Option<String>,
    repair: impl FnOnce() -> Result<bool>,
    mut close_terminal: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    let _budget = budget::Scope::enter(deadline);
    crate::native::session::close::budget::close_remaining(deadline)?;
    let repair_error = repair()
        .err()
        .map(|error| format!("pre-close session repair failed: {error:#}"));
    if let Err(expired) = budget::check() {
        return Err(match repair_error {
            Some(error) => anyhow::anyhow!(error).context(expired),
            None => expired,
        });
    }
    let mut outcome = terminal::CloseOutcome::Missing;
    close_session_state_with_error(
        store,
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
    mut close_error: Option<String>,
    mut close_terminal: F,
    deadline: std::time::Instant,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseResult>,
{
    let _budget = budget::Scope::enter(deadline);
    crate::native::session::close::budget::close_remaining(deadline)?;
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let _turn_lock = store.lock()?;
    let status: SessionStatus = store.status()?;
    if status.state == SessionState::Closed {
        let consume_result = consume_terminal_handle(store, None, close_error.as_deref());
        let close_result =
            mark_session_closed_locked(store, &claim_path, close_error.or(status.error));
        consume_result?;
        return close_result;
    }

    let terminal_path = store.record(CoreRecord::Terminal).path().to_owned();
    let closing = store.record(CoreRecord::TerminalClosing);
    let closing_path = closing.path().to_owned();
    if store.record(CoreRecord::TerminalClosed).present()? {
        close_error = merge_close_reason(close_error, terminal_consumption_reason(store)?);
        let consume_result = consume_terminal_handle(store, None, close_error.as_deref());
        let close_result = mark_session_closed_locked(store, &claim_path, close_error);
        consume_result?;
        return close_result;
    }
    // No ownership gate sees a handle-less session. Fence this close while the
    // pending launcher can still return a surface, under the same lifecycle lock.
    if status.state == SessionState::Launching
        && !store.record(CoreRecord::Terminal).present()?
        && !closing.present()?
        && store.closed_if_present()?.is_none()
        && let Some(record) = launch::read(store)?
        && record.phase == launch::Phase::Pending
        && record.deadline_unix_ms > unix_ms()
    {
        bail!(
            "the launcher is still creating the surface for this session (launch deadline {}); no handle exists yet and nothing was closed; close again after the deadline or once the session has failed",
            record.deadline_unix_ms
        );
    }
    match store.claim_terminal_handle()? {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !closing.present()? {
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
                return mark_session_closed_locked(store, &claim_path, close_error);
            }
            // A prior closer may have stopped after atomically claiming the handle but before
            // invoking the terminal adapter. The turn-claim lock serializes recovery, so resume
            // that durable close transaction instead of reporting success with a live surface.
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim terminal handle {}",
                    terminal_path.display()
                )
            });
        }
    }

    let terminal = match closing.json_with(super::record::terminal) {
        Ok(terminal) => terminal,
        Err(error) => {
            restore_terminal_handle(&closing_path, &terminal_path)?;
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

    let consume_result =
        consume_terminal_handle(store, Some(terminal.kind.as_str()), close_error.as_deref());
    // Only this path has the adapter's positive close/absence result. No-handle
    // close, repair, and replay of a consumed handle cannot clear the observation.
    let cleared = status.residual_surface().map(|_| ResidualSurface::Cleared);
    let close_result =
        mark_session_closed_with_residual_locked(store, &claim_path, close_error, cleared);
    consume_result?;
    close_result
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

// Every OS records the close's reason in the consumption marker, so that a close interrupted
// after it and before the closed record keeps that reason (os-parity).
fn terminal_consumption_reason(store: &Store) -> Result<Option<String>> {
    // Bytes that are not UTF-8 carry no reason, as malformed JSON carries none: the read is
    // strict, and an undecodable tombstone never keeps a close from finishing.
    Ok(store
        .record(CoreRecord::TerminalClosed)
        .bytes()?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| super::record::terminal_consumption_reason(&text)))
}

fn merge_close_reason(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) if first != second => Some(format!("{first}; {second}")),
        (first, second) => first.or(second),
    }
}

fn consume_terminal_handle(
    store: &Store,
    terminal_kind: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let mut marker = serde_json::json!({
        "consumed": true,
        "terminal": terminal_kind,
    });
    if let Some(reason) = reason {
        marker["reason"] = reason.into();
    }
    let tombstone_result = store.write_terminal_closed(&marker);
    #[cfg(test)]
    if tombstone_result.is_ok() {
        interruption::at(interruption::Point::ConsumptionMarkerWritten);
    }
    // A consumption marker is durable authority for the next close. After it is
    // written, expiry leaves the claim in place; this is never a handle rollback.
    budget::check()?;
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

// Repair settles under the lock it already holds; the tests settle a session directly.
#[cfg(test)]
fn mark_session_closed(store: &Store, error: Option<String>) -> Result<()> {
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let _lock = store.lock()?;
    mark_session_closed_locked(store, &claim_path, error)
}

fn mark_session_closed_locked(
    store: &Store,
    claim_path: &Path,
    error: Option<String>,
) -> Result<()> {
    mark_session_closed_with_residual_locked(store, claim_path, error, None)
}

fn mark_session_closed_with_residual_locked(
    store: &Store,
    claim_path: &Path,
    error: Option<String>,
    residual_surface: Option<ResidualSurface>,
) -> Result<()> {
    let directory = store.directory();
    // Repair can reach this after consumption but before the closed tombstone.
    // The consumption marker carries the callback's reason across that boundary.
    let error = merge_close_reason(error, terminal_consumption_reason(store)?);
    // A refused presence check fails the consumption step, reported after the status update
    // as a failed consumption is.
    let consume_result = (|| {
        if store.record(CoreRecord::Terminal).present()?
            || store.record(CoreRecord::TerminalClosing).present()?
        {
            consume_terminal_handle(store, None, error.as_deref())
        } else {
            Ok(())
        }
    })();
    let status_result = update_status_with_residual(
        directory,
        SessionState::Closed,
        None,
        error,
        residual_surface,
    );
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
    let directory = store.directory();
    // Completion recovery runs first so a live owner's finished turn is published before
    // anything else is decided. Its failure is damage (a missing `events/`, a journal
    // whose event cannot be compared), not a reason to leave a dead owner's session
    // installed forever: the owner check still runs, a dead owner's session is closed as
    // it would be without the damage (the close settles the journal without publishing),
    // and the damage is reported in the close error. Under a live owner, or when the
    // owner cannot be shown dead, the damage is the result.
    let recovery_damage = recover_pending_completion(directory).err();
    // A bounded wait is not journal damage and grants no repair authority.
    if let Err(expired) = budget::check() {
        return Err(recovery_damage.unwrap_or(expired));
    }
    let untouched = |damage: Option<anyhow::Error>| match damage {
        Some(error) => Err(error),
        None => Ok(false),
    };
    if launch::repair(store)? {
        return untouched(recovery_damage);
    }
    let status: SessionStatus = store.status()?;
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
            // The scan ran without the lifecycle lock; an explicit close that holds it can
            // have restored its claimed handle meanwhile. Look again under the lock, so that
            // a restored handle is never consumed by this settlement.
            let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
            let _lock = store.lock()?;
            if !repair_examines_owner(&store.status()?.state) || retention.retains(store)? {
                return untouched(recovery_damage);
            }
            mark_session_closed_locked(store, &claim_path, repair_error)?;
        }
        RepairSurface::CloseOnWindows(mut close) => {
            // Reuse the explicit-close claim so an interrupted attempt can be resumed
            // and concurrent repair cannot close the same surface twice.
            close_session_state_with_error(
                store,
                repair_error,
                |surface| close.close(surface, deadline).map(Into::into),
                deadline,
            )
            .context(close.failure_context())?;
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
