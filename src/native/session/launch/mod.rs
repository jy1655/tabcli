//! Provider-neutral startup receipt and fencing. A terminal accepting a command is not
//! evidence that its wrapper ran. The lifecycle lock serializes cancellation with spawn.
use crate::native::session::primitives::unix_ms;
use crate::native::session::state::{
    ResidualSurface, SessionState, SessionStatus, update_status, update_status_with_residual,
};

use crate::native::clock::Clock;
use crate::native::failure_kind::TimedOut;
use crate::native::session::turn::{recover_pending_completion_locked, remove_turn_claim_locked};
use crate::native::session::{CoreRecord, Reader, RecordReader, RecordStore, Store, record, turn};
use crate::native::terminal;
#[cfg(unix)]
use crate::native::terminal::shell_quote;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Child;
use std::{
    process::Command,
    time::{Duration, Instant},
};

#[cfg(test)]
// macOS and Windows tests address the launch record by this name; production code uses
// the store.
#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
pub(in crate::native) use super::names::LAUNCH_FILE as FILE;
pub(in crate::native) const LOG: &str = "launch.log";
pub(in crate::native) mod host;
pub(in crate::native) mod launcher;
pub(in crate::native) mod pending;
pub(in crate::native) mod wrapper;
// Written at the end of the launch log by whoever sees the wrapper end: the bootstrap for a
// wrapper that could not write, the wrapper itself, and the admission refusal.
pub(in crate::native) const WRAPPER_EXIT_CODE_MARKER: &str = "wrapper_exit_code=";
pub(in crate::native) use super::state::RESIDUAL_SURFACE_MARKER;
const START_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod admission_tests;
#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
pub(in crate::native) mod binding_tests;
#[cfg(test)]
pub(in crate::native) mod test_support;
#[cfg(test)]
mod wait_tests;

pub(in crate::native) fn install_script(store: &Store, contents: &str) -> Result<String> {
    let directory = store.directory();
    // A new terminal can still be in canonical input mode while its shell starts. A
    // long write-text command was truncated there in macOS LIVE (#50). Send only a
    // quoted script path; source it in the original shell to preserve owner/TTY identity.
    #[cfg(unix)]
    {
        let path = directory.join("launch.sh");
        RecordStore::at(&path).write_private(contents.as_bytes())?;
        Ok(format!(". {}", shell_quote(path.as_os_str())))
    }
    #[cfg(not(unix))]
    {
        // Windows passes -Command directly to its console process, not through a TTY
        // input buffer. Keep it native; sourcing a .ps1 would add an execution-policy gate.
        let _ = directory;
        Ok(contents.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::native) enum Phase {
    Pending,
    // Durable before spawn: a crash here cannot prove that no child exists.
    Spawning,
    Spawned,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::native) struct Record {
    pub(in crate::native) schema: u32,
    pub(in crate::native) claim_token: String,
    pub(in crate::native) deadline_unix_ms: u128,
    pub(in crate::native) phase: Phase,
}

// The launcher's deadline settles a launch that is still unconfirmed; the wrapper's later
// bound only ends a version re-check that is still running, so the launcher's failure is the
// one recorded. The grace orders the two and guarantees nothing: a launcher delayed past it
// can still find the wrapper's failure recorded first, or the lock held by its settlement.
const RECHECK_GRACE: Duration = Duration::from_secs(2);

impl Record {
    /// The time from the wall clock's reading here until `after` past the recorded deadline:
    /// zero once that time has passed, and `None` when it is more than `u64::MAX` milliseconds
    /// away. This is the wall-clock half of the receipt's conversion to the monotonic clock;
    /// each caller reads the monotonic clock itself, in its own order around this reading, and
    /// keeps its own cap, grace and overflow policy.
    pub(in crate::native) fn remaining(&self, after: Duration) -> Option<Duration> {
        let remaining = self
            .deadline_unix_ms
            .saturating_add(after.as_millis())
            .saturating_sub(unix_ms());
        u64::try_from(remaining).ok().map(Duration::from_millis)
    }

    // The wrapper's re-check bound: the recorded deadline plus the grace. The wall clock is
    // read before `monotonic_now`, so that a pause between the two readings can only lengthen
    // the bound. A remaining time that does not fit the conversion or the monotonic clock
    // gives none, and the wrapper keeps its unbounded check.
    fn recheck_bound(&self, monotonic_now: impl FnOnce() -> Instant) -> Option<Instant> {
        let remaining = self.remaining(RECHECK_GRACE)?;
        monotonic_now().checked_add(remaining)
    }
}

/// `now` plus a receipt's `remaining` time, saturating: a remaining time too long for the
/// conversion counts as `u64::MAX` milliseconds, and an instant the monotonic clock cannot
/// represent is the furthest one short of it that it can. The macOS launch host and binding
/// wait use it.
#[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
pub(in crate::native) fn saturating_deadline(now: Instant, remaining: Option<Duration>) -> Instant {
    let remaining = remaining.unwrap_or(Duration::from_millis(u64::MAX));
    if let Some(deadline) = now.checked_add(remaining) {
        return deadline;
    }
    let (mut reachable, mut beyond) = (Duration::ZERO, remaining);
    while beyond - reachable > Duration::from_nanos(1) {
        let middle = reachable + (beyond - reachable) / 2;
        if now.checked_add(middle).is_some() {
            reachable = middle;
        } else {
            beyond = middle;
        }
    }
    now + reachable
}

pub(in crate::native) fn read(reader: &Reader) -> Result<Option<Record>> {
    reader
        .record(CoreRecord::Launch)
        .bytes()?
        .map(|bytes| {
            record::launch(&bytes).map_err(|error| error.context("invalid launch receipt"))
        })
        .transpose()
}

/// Adapters declare binding diagnostics and budget; the shared wait owns observation order.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(in crate::native) struct BindingFacts {
    pub(in crate::native) host: &'static str,
    pub(in crate::native) cancelled: &'static str,
    pub(in crate::native) budget: BindingBudget,
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(in crate::native) enum BindingBudget {
    // Only macOS kinds measure their binding from the start timeout.
    #[cfg_attr(any(target_os = "linux", windows), allow(dead_code))]
    StartTimeout,
    RemainingReceiptCapped,
}

/// Observe a pending launch until its managed surface is bound. This is an ordered,
/// unlocked observation, not authorization to spawn; `spawn` owns that final fence. The
/// budget and every retry read `clock`; the host passes the system clock.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(in crate::native) fn wait_for_binding(
    reader: &Reader,
    session_id: &str,
    facts: &BindingFacts,
    clock: &mut dyn Clock,
) -> Result<terminal::TerminalSession> {
    let name = facts.host;
    let cancelled = facts.cancelled;
    let initial = read(reader)?.with_context(|| format!("missing {name} launch receipt"))?;
    #[cfg(test)]
    binding_tests::checkpoint(binding_tests::Point::InitialReceipt);
    // Preserve both budgets: after wall-clock rollback or a receipt extension,
    // The start-timeout budget may continue beyond the initial receipt's remaining duration.
    let deadline = match facts.budget {
        BindingBudget::StartTimeout => clock.now() + START_TIMEOUT,
        BindingBudget::RemainingReceiptCapped => {
            // The wall clock before the monotonic clock: a pause between the two readings can
            // only lengthen the budget, and the receipt's own expiry still bounds the wait.
            let remaining = initial.remaining(Duration::ZERO);
            let now = clock.now();
            saturating_deadline(now, remaining).min(now + START_TIMEOUT)
        }
    };
    #[cfg(test)]
    binding_tests::checkpoint(binding_tests::Point::BudgetCaptured);
    loop {
        let record = read(reader)?.with_context(|| format!("missing {name} launch receipt"))?;
        // The judgment reads the status even after expiry and the Claim last, and the binding
        // is read only for a pending launch: this order also determines which damaged-record
        // error the host reports.
        let mut budget_spent = || clock.now() >= deadline;
        if pending::judge(
            reader,
            &record,
            &initial.claim_token,
            pending::Expiry::BudgetOrReceipt(&mut budget_spent),
        )?
        .is_err()
        {
            bail!("{cancelled}");
        }
        #[cfg(test)]
        binding_tests::checkpoint(binding_tests::Point::BeforeBinding);
        if let Some(surface) = pending::bound_surface(reader, session_id, || {
            format!("invalid {name} surface binding")
        })? {
            return Ok(surface);
        }
        // The real wait sleeps a full 10 ms, even with less than 10 ms remaining.
        clock.sleep(Duration::from_millis(10));
    }
}

pub(in crate::native) fn log(store: &Store, message: &str) {
    let directory = store.directory();
    // Diagnostics are auxiliary. A missing/unwritable log must never stop valid input.
    let path = Reader::open_unchecked(directory)
        .private(LOG)
        .path()
        .to_owned();
    let _ = (|| -> Result<()> {
        if !path.exists() {
            let _ = RecordStore::at(&path).write_private(b"");
        }
        if !RecordReader::at(&path).is_regular_file()? {
            return Ok(());
        }
        // One write per line: the launcher and the wrapper append at the same time.
        RecordStore::at(&path).append(|| format!("{} {message}\n", unix_ms()).into_bytes())?;
        Ok(())
    })();
}

pub(in crate::native) fn begin(
    store: &Store,
    token: &str,
    request_deadline: Instant,
) -> Result<Instant> {
    let directory = store.directory();
    let budget = request_deadline
        .saturating_duration_since(Instant::now())
        .min(START_TIMEOUT);
    let deadline = Instant::now() + budget;
    let _lock = Store::open_unchecked(directory).lock()?;
    Store::open_unchecked(directory)
        .record(CoreRecord::Launch)
        .write_json(&Record {
            schema: 1,
            claim_token: token.to_owned(),
            deadline_unix_ms: unix_ms() + budget.as_millis(),
            phase: Phase::Pending,
        })?;
    log(store, "launch_pending; waiting for provider spawn");
    Ok(deadline)
}

fn fail_locked(store: &Store, record: &Record, reason: &str) -> Result<()> {
    fail_locked_with_residual(store, record, reason, None)
}

pub(in crate::native) fn fence_failed_start(
    store: &Store,
    expected_token: &str,
    reason: &str,
) -> Result<()> {
    let _lock = store.lock()?;
    fence_failed_start_locked(store, expected_token, reason)
}

pub(in crate::native) fn retry_failed_start_fence(
    store: &Store,
    expected_token: &str,
    reason: &str,
) -> Result<()> {
    let Some(_lock) = store.try_lock()? else {
        return Ok(());
    };
    fence_failed_start_locked(store, expected_token, reason)
}

fn fence_failed_start_locked(store: &Store, expected_token: &str, reason: &str) -> Result<()> {
    let status = store.status()?;
    if status.state != SessionState::Launching
        || turn::current_claim_token(store)?.as_deref() != Some(expected_token)
    {
        return Ok(());
    }
    let reason = status.error.map_or_else(
        || reason.to_owned(),
        |earlier| format!("{earlier}; {reason}"),
    );
    if let Ok(Some(record)) = read(store)
        && record.claim_token == expected_token
    {
        return fail_locked(store, &record, &reason);
    }
    update_status(store.directory(), SessionState::Failed, None, Some(reason))
}

/// Settles a fresh start whose launch failed before the launcher fenced its Claim, as a
/// fenced launch failure is settled. The Claim is fenced first, so that this settlement and
/// not the Claim's Drop owns its release. Under the lifecycle lock, and only while the
/// Session is still launching with this Claim current, a receipt for the Claim fails through
/// the launch failure publication; without one, the status alone records the failure (the
/// receipt's read error follows the launch error when the receipt cannot be read) and the
/// Claim is released. A settlement that cannot write the failure keeps the Claim installed,
/// so that repair past the receipt's deadline still settles it. Returns the launch error,
/// with the settlement's own failure as context when it did not complete.
pub(in crate::native) fn settle_unfenced_start(
    store: &Store,
    claim: &mut turn::Claim,
    error: anyhow::Error,
) -> anyhow::Error {
    claim.fence_launch();
    let reason = format!("{error:#}");
    let settled = store
        .lock()
        .and_then(|_lock| settle_unfenced_start_locked(store, claim.token(), &reason));
    match settled {
        Ok(()) => error,
        Err(settlement) => error.context(format!(
            "the failed launch could not be settled: {settlement:#}"
        )),
    }
}

fn settle_unfenced_start_locked(store: &Store, token: &str, reason: &str) -> Result<()> {
    if turn::current_claim_token(store)?.as_deref() != Some(token) {
        return Ok(());
    }
    let claim_path = Reader::open_unchecked(store.directory())
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    if store.status()?.state != SessionState::Launching {
        // Another writer ended the Session first; release the Claim as its Drop would.
        return remove_turn_claim_locked(&claim_path);
    }
    let reason = match read(store) {
        Ok(Some(record)) if record.claim_token == token => {
            return fail_locked(store, &record, reason);
        }
        Ok(_) => reason.to_owned(),
        Err(receipt_error) => format!("{reason}; {receipt_error:#}"),
    };
    update_status(store.directory(), SessionState::Failed, None, Some(reason))?;
    remove_turn_claim_locked(&claim_path)
}

pub(in crate::native) enum FailedStartEvidence {
    NoUnresolvedSpawn,
    SpawnAttempted,
    Pending(String),
}

pub(in crate::native) fn failed_start_evidence_locked(
    store: &Store,
    expected_token: &str,
) -> Result<FailedStartEvidence> {
    let pending = |reason: &str| Ok(FailedStartEvidence::Pending(reason.to_owned()));
    let state = store.status()?.state;
    if state.accepts_prompt() || state == SessionState::Working {
        return pending("the session can already accept or execute prompts");
    }
    if turn::current_claim_token(store)?.is_some_and(|token| token != expected_token) {
        return pending("the current claim belongs to another request");
    }
    let Some(record) = read(store)? else {
        return pending("the failed launch has no receipt");
    };
    if record.claim_token != expected_token {
        return pending("the launch receipt belongs to another claim");
    }
    match record.phase {
        Phase::Pending if state.ended() => {
            if store.record(CoreRecord::ProviderProcess).bytes()?.is_none() {
                Ok(FailedStartEvidence::NoUnresolvedSpawn)
            } else {
                Ok(FailedStartEvidence::SpawnAttempted)
            }
        }
        Phase::Pending => pending("the pending launch is not fenced by an ended status"),
        Phase::Spawning | Phase::Spawned => Ok(FailedStartEvidence::SpawnAttempted),
    }
}

fn fail_locked_with_residual(
    store: &Store,
    record: &Record,
    reason: &str,
    residual_surface: Option<ResidualSurface>,
) -> Result<()> {
    let directory = store.directory();
    if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
        != Some(&record.claim_token)
    {
        return Ok(());
    }
    let status: SessionStatus = store.status()?;
    if status.state != SessionState::Launching {
        if status.state == SessionState::Failed && record.phase == Phase::Pending {
            remove_turn_claim_locked(
                Reader::open_unchecked(directory)
                    .record(CoreRecord::TurnClaim)
                    .path(),
            )?;
        }
        return Ok(());
    }
    let reason = if record.phase == Phase::Spawning {
        format!("{reason}; provider spawn is uncertain; the claim is retained, do not resend")
    } else {
        reason.to_owned()
    };
    update_status_with_residual(
        directory,
        SessionState::Failed,
        None,
        Some(reason.clone()),
        residual_surface,
    )?;
    if record.phase == Phase::Pending {
        remove_turn_claim_locked(
            Reader::open_unchecked(directory)
                .record(CoreRecord::TurnClaim)
                .path(),
        )?;
    }
    log(store, &reason);
    Ok(())
}

// The plain form, which the tests of this module and of close use.
#[cfg(test)]
pub(in crate::native) fn fail(store: &Store, reason: &str) -> Result<()> {
    fail_deciding(store, reason, |_, reason| Ok(reason.to_owned()))
}

// `fail`, with the recorded wording decided under the lock from the launch record
// and the other records as they are at that moment.
fn fail_deciding(
    store: &Store,
    reason: &str,
    wording: impl FnOnce(&Record, &str) -> Result<String>,
) -> Result<()> {
    let Some(_lock) = store.try_lock()? else {
        bail!("{reason}; provider startup is in progress; the claim is retained");
    };
    if let Some(record) = read(store)?
        && record.phase != Phase::Spawned
    {
        let reason = wording(&record, reason)?;
        fail_locked(store, &record, &reason)?;
    }
    Ok(())
}

// The error returned by ask, including its JSON error rendering.
pub(in crate::native) fn terminal_failure(store: &Store, error: anyhow::Error) -> anyhow::Error {
    let retained = error.downcast_ref::<terminal::RetainedLaunchSurface>();
    let unverified = retained.is_some_and(|surface| surface.unverified_cleanup);
    let has_surface = retained.is_some();
    let handoff = terminal_failed(store, &error);
    let residual = unverified || (has_surface && handoff.is_err());
    let error = match handoff {
        Ok(()) => error,
        Err(record_error) => {
            error.context(format!("terminal launch failure handoff: {record_error:#}"))
        }
    };
    if residual && !format!("{error:#}").contains(RESIDUAL_SURFACE_MARKER) {
        error.context(RESIDUAL_SURFACE_MARKER)
    } else {
        error
    }
}

// The launcher's terminal-creation failure handoff.
pub(in crate::native) fn terminal_failed(store: &Store, error: &anyhow::Error) -> Result<()> {
    let reason = format!("terminal launch failed: {error:#}");
    let Some(retained) = error.downcast_ref::<terminal::RetainedLaunchSurface>() else {
        // Only a failure before a surface was bound (a close in progress moves the handle
        // to the closing record and may give it back) and before the provider spawn began
        // proves that nothing was sent. A bound surface keeps its handle, and a spawn in
        // progress has its own wording. Decided under the lock, on the records as they are.
        return fail_deciding(store, &reason, |record, reason| {
            let reader = Reader::open_unchecked(store.directory());
            // Only the records' presence counts here; their content is not interpreted.
            let unbound = reader.record(CoreRecord::Terminal).bytes()?.is_none()
                && reader
                    .record(CoreRecord::TerminalClosing)
                    .bytes()?
                    .is_none();
            Ok(if unbound && record.phase == Phase::Pending {
                format!("{reason}; no surface handle was recorded and no prompt was sent")
            } else {
                reason.to_owned()
            })
        });
    };
    let _lock = store.lock()?;
    let evidence = (|| -> Result<_> {
        Ok((
            store.status()?,
            read(store)?,
            turn::current_claim_token(store)?,
            store.closed_if_present()?,
        ))
    })();
    let (status, record, claim, closed) = match evidence {
        Ok(evidence) => evidence,
        Err(error) => {
            let message = format!(
                "{reason}; exact retained surface={:?}; {RESIDUAL_SURFACE_MARKER}; launch handoff evidence could not be read: {error:#}",
                retained.surface
            );
            // Failed evidence grants no bind or lifecycle transition. Preserve the
            // residual warning if the status/tombstone can still be read and amended.
            store
                .record_residual_surface(&message)
                .with_context(|| message.clone())?;
            bail!("{message}");
        }
    };
    let owns_claim = record
        .as_ref()
        .is_some_and(|record| claim.as_deref() == Some(record.claim_token.as_str()));
    if status.state != SessionState::Launching || !owns_claim || closed.is_some() {
        let disposition = if status.state == SessionState::Closed || closed.is_some() {
            "the session was closed during the launch"
        } else {
            "the session is no longer owned by this launch"
        };
        let message = format!(
            "{disposition}; exact retained surface={:?}; {RESIDUAL_SURFACE_MARKER}; {reason}",
            retained.surface
        );
        log(store, &message);
        // Diagnostic-only amendment: never bind or reopen a cancelled launch. The
        // tombstone must carry the warning too, or later status convergence erases it.
        // Preserve state, generation, timestamps and exit code under the status lock.
        store
            .record_residual_surface(&message)
            .with_context(|| message.clone())?;
        bail!("{message}");
    }
    let mut surface = retained.surface.clone();
    surface.managed_session_id = Some(
        store
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .context("session directory has no session id")?
            .to_owned(),
    );
    let persisted = store.write_terminal(&surface);
    let residual =
        (retained.unverified_cleanup || persisted.is_err()).then_some(ResidualSurface::Unverified);
    let reason = if residual.is_some() && !reason.contains(RESIDUAL_SURFACE_MARKER) {
        format!("{reason}; {RESIDUAL_SURFACE_MARKER}")
    } else {
        reason
    };
    // Even a failed record write must leave the launch failure and exact surface in
    // status.error. Return the write error as well; never claim persistence succeeded.
    if let Some(record) = record {
        fail_locked_with_residual(store, &record, &reason, residual)?;
    }
    persisted.context("failed to persist the retained launch surface")
}

fn confirmed(reader: &Reader, record: &Record, status: &SessionStatus) -> bool {
    record.phase == Phase::Spawned
        || (record.phase == Phase::Spawning
            && matches!(
                status.state,
                SessionState::Running
                    | SessionState::AwaitingInitialInput
                    | SessionState::Working
                    | SessionState::Ready
            )
            && reader.provider_process().is_ok_and(|p| {
                reader.directory().file_name().and_then(|n| n.to_str())
                    == Some(p.managed_session_id.as_str())
            }))
}

/// One lifecycle-locked decision for wait and repair. Current confirmed/ended state
/// wins over a retained diagnostic; an old claim can never settle its successor.
#[derive(Debug, PartialEq, Eq)]
enum HostDecision {
    Confirmed,
    Ended,
    Failed,
    Pending,
}
fn host_decision_locked(store: &Store) -> Result<HostDecision> {
    let Some(receipt) = read(store)? else {
        return Ok(HostDecision::Pending);
    };
    let status = store.status()?;
    if status.state.ended() || store.closed_if_present()?.is_some() {
        return Ok(HostDecision::Ended);
    }
    if confirmed(store, &receipt, &status) {
        return Ok(HostDecision::Confirmed);
    }
    let claim = turn::current_claim_token(store)?;
    if status.state != SessionState::Launching
        || claim.as_deref() != Some(receipt.claim_token.as_str())
    {
        return Ok(HostDecision::Pending);
    }
    if let host::HostObservation::Recorded(record) = host::observe(store, Some(&receipt))
        && let Some(failure) = &record.failure
    {
        use host::Responsibility::*;
        let mut message = match failure.reason.responsibility() {
            StartupFiles => "terminal shell environment could not be prepared",
            Deadline | SurfaceIdentity | HostStep => "terminal launch host failed",
        }
        .to_owned();
        if let Some(detail) = record.failure_detail() {
            message.push_str(&format!(": {detail}"));
        }
        fail_locked(
            store,
            &receipt,
            &format!(
                "{message} ({}); last recorded launch stage: {}",
                failure.reason.word(),
                record.stage.word()
            ),
        )?;
        return Ok(HostDecision::Failed);
    }
    Ok(HostDecision::Pending)
}
fn deadline_reason(reader: &Reader, receipt: Option<&Record>) -> String {
    match host::observe(reader, receipt) {
        host::HostObservation::Recorded(record) => format!(
            "{LAUNCH_DEADLINE}; last recorded launch stage: {}",
            record.stage.word()
        ),
        _ => LAUNCH_DEADLINE.into(),
    }
}

/// Called before old dead-owner repair. A new launch with an uncertain spawn must not
/// be silently closed/released by the legacy owner-only repair path.
pub(in crate::native) fn repair(store: &Store) -> Result<bool> {
    repair_with_owner(store, super::owner::attest_recorded_owner)
}

fn repair_with_owner(
    store: &Store,
    observe: impl FnOnce(&Reader) -> Result<Option<super::owner::OwnerAttestation>>,
) -> Result<bool> {
    let Some(record) = read(store)? else {
        return Ok(false);
    };
    let status: SessionStatus = store.status()?;
    if confirmed(store, &record, &status) {
        return Ok(false);
    }
    let Some(_lock) = store.try_lock()? else {
        return Ok(true);
    };
    let Some(record) = read(store)? else {
        return Ok(false);
    };
    let status: SessionStatus = store.status()?;
    if confirmed(store, &record, &status) {
        return Ok(false);
    }
    match host_decision_locked(store)? {
        HostDecision::Confirmed => return Ok(false),
        HostDecision::Failed => return Ok(true),
        HostDecision::Ended if store.closed_if_present()?.is_some() => return Ok(false),
        HostDecision::Ended | HostDecision::Pending => {}
    }
    if status.state == SessionState::Closed {
        return Ok(false);
    }
    if status.state == SessionState::Launching {
        if unix_ms() >= record.deadline_unix_ms {
            fail_locked(store, &record, &deadline_reason(store, Some(&record)))?;
        } else if let Some(owner) = observe(store)? {
            use crate::native::process_identity::ProcessObservation;
            match owner.process {
                ProcessObservation::Gone(_) => fail_locked(
                    store,
                    &record,
                    "native launch wrapper exited before startup was confirmed",
                )?,
                ProcessObservation::Unknown(reason) => {
                    bail!("cannot verify native launch wrapper: {reason}");
                }
                ProcessObservation::Live(_) => {}
            }
        }
    } else if status.state == SessionState::Failed {
        fail_locked(
            store,
            &record,
            status.error.as_deref().unwrap_or("provider launch failed"),
        )?;
    }
    Ok(true)
}

const LAUNCH_DEADLINE: &str = "provider launch timed out before startup was confirmed";
const SURFACE_DISAPPEARED: &str =
    "terminal surface disappeared before provider launch was confirmed";
/// The wait inspects the surface this often, first one interval after it starts.
const SURFACE_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// One inspection may take this long, and never past the deadline.
const SURFACE_PROBE_BOUND: Duration = Duration::from_secs(2);
/// The longest sleep between two readings of the records.
const WAIT_POLL: Duration = Duration::from_millis(100);

/// The surface observation port of the launcher's wait: whether the launch's terminal
/// surface is still present. An inspection error does not prove absence.
trait SurfacePresence {
    fn present(&mut self, timeout: Duration) -> Result<bool>;
}

/// Production: the terminal's read-only presence check of the launch's surface.
struct TerminalSurface<'a> {
    directory: &'a std::path::Path,
    surface: &'a terminal::TerminalSession,
}

impl SurfacePresence for TerminalSurface<'_> {
    fn present(&mut self, timeout: Duration) -> Result<bool> {
        terminal::surface_present(self.directory, self.surface, timeout)
    }
}

pub(in crate::native) fn wait(
    store: &Store,
    surface: &terminal::TerminalSession,
    deadline: Instant,
) -> Result<()> {
    wait_with(
        store,
        deadline,
        &mut crate::native::clock::SystemClock,
        &mut TerminalSurface {
            directory: store.directory(),
            surface,
        },
    )
}

/// The launcher's wait for a confirmed launch. Each poll reads the records, then decides on
/// the time and the surface: at the deadline, or when a due probe finds the surface gone,
/// the wait settles the launch under the lifecycle lock. Every reading of time and every
/// sleep goes through `clock`.
fn wait_with(
    store: &Store,
    deadline: Instant,
    clock: &mut dyn Clock,
    presence: &mut dyn SurfacePresence,
) -> Result<()> {
    let mut next_probe = clock.now() + SURFACE_PROBE_INTERVAL;
    loop {
        if recorded_confirmation(store)? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(clock.now());
        let deadline_reached = remaining.is_zero();
        let reason = if deadline_reached {
            Some(deadline_reason(store, read(store)?.as_ref()))
        } else if clock.now() >= next_probe {
            next_probe = clock.now() + SURFACE_PROBE_INTERVAL;
            match presence.present(remaining.min(SURFACE_PROBE_BOUND)) {
                Ok(false) => Some(SURFACE_DISAPPEARED.to_owned()),
                // Inspection errors (including permissions/timeouts) do not prove absence.
                Ok(true) | Err(_) => None,
            }
        } else {
            None
        };
        if let Some(reason) = reason {
            return settle(store, reason, deadline_reached);
        }
        clock.sleep(remaining.min(WAIT_POLL));
    }
}

// What the records already decide for the wait: `true` for a confirmed launch, an error
// naming how an ended or failed launch ended, `false` while it is pending. The locked host
// decision runs only when the lifecycle lock is free; the status and the receipt are read
// on every poll.
fn recorded_confirmation(store: &Store) -> Result<bool> {
    if let Some(_lock) = store.try_lock()? {
        match host_decision_locked(store)? {
            HostDecision::Confirmed => return Ok(true),
            HostDecision::Failed => bail!(
                "{}",
                store
                    .status()?
                    .error
                    .unwrap_or_else(|| "launch host failed".into())
            ),
            HostDecision::Ended => {
                let status = store.status()?;
                bail!(
                    "{}",
                    status.error.unwrap_or_else(|| format!(
                        "launch entered {}",
                        if store.closed_if_present().ok().flatten().is_some() {
                            SessionState::Closed
                        } else {
                            status.state
                        }
                    ))
                );
            }
            HostDecision::Pending => {}
        }
    }
    let status: SessionStatus = store.status()?;
    if status.state.ended() {
        bail!(
            "{}",
            status
                .error
                .unwrap_or_else(|| format!("launch entered {}", status.state))
        );
    }
    Ok(read(store)?.is_some_and(|record| confirmed(store, &record, &status)))
}

// Settles the wait for `reason` under the lifecycle lock: a launch confirmed or settled
// meanwhile keeps its own outcome; otherwise the launch fails with `reason` (the deadline's
// reason read again under the lock). A held lock leaves the claim and the records alone.
fn settle(store: &Store, reason: String, deadline_reached: bool) -> Result<()> {
    let Some(_lock) = store.try_lock()? else {
        let message = format!(
            "{reason}; startup is still holding the lifecycle lock, execution is uncertain; the claim is retained"
        );
        if deadline_reached {
            bail!(TimedOut::new(message));
        }
        bail!("{message}");
    };
    let record = read(store)?.context("launch receipt disappeared")?;
    let status: SessionStatus = store.status()?;
    if confirmed(store, &record, &status) {
        return Ok(());
    }
    match host_decision_locked(store)? {
        HostDecision::Confirmed => return Ok(()),
        HostDecision::Failed | HostDecision::Ended => bail!(
            "{}",
            store
                .status()?
                .error
                .unwrap_or_else(|| "launch has ended".into())
        ),
        HostDecision::Pending => {}
    }
    let reason = if deadline_reached {
        deadline_reason(store, Some(&record))
    } else {
        reason
    };
    fail_locked(store, &record, &reason)?;
    let status: SessionStatus = store.status()?;
    let message = status.error.as_deref().unwrap_or(&reason);
    // The hint needs this wait's own deadline and that reason as the recorded text;
    // another writer's status text stays a plain message.
    if deadline_reached && message == reason {
        bail!(TimedOut::new(message));
    }
    bail!("{message}");
}

fn check_spawn_locked(store: &Store, record: Option<&Record>) -> Result<()> {
    let judged = match record {
        Some(record) => {
            pending::judge(store, record, &record.claim_token, pending::Expiry::Receipt)?
        }
        // A launch without a receipt is judged on its status and tombstone alone.
        None => pending::launching(store)?,
    };
    match judged {
        Ok(()) => Ok(()),
        Err(pending::NotPending::State(state)) => {
            bail!("provider launch cancelled: session is {state}")
        }
        // The tombstone is judged only for a launching status.
        Err(pending::NotPending::Closed) => bail!(
            "provider launch cancelled: session is {}",
            SessionState::Launching
        ),
        Err(pending::NotPending::Phase(_)) => {
            bail!("provider launch already attempted; refusing a second spawn")
        }
        Err(pending::NotPending::Expired) => {
            bail!("provider launch deadline expired before spawn")
        }
        Err(pending::NotPending::ReceiptClaim | pending::NotPending::Claim) => {
            bail!("provider launch claim no longer belongs to this wrapper")
        }
    }
}

// `spawn_granted` for a command without a trust-bearing option, as the tests start one.
#[cfg(test)]
pub(in crate::native) fn spawn<F>(
    store: &Store,
    command: &mut Command,
    record_process: F,
) -> Result<Child>
where
    F: FnOnce(&mut Child) -> Result<()>,
{
    spawn_granted(store, command, None, record_process)
}

/// `spawn` for a command that may carry a trust-bearing launch option (`grant`, from its plan).
pub(in crate::native) fn spawn_granted<F>(
    store: &Store,
    command: &mut Command,
    grant: Option<&crate::native::consent::LaunchGrant>,
    record_process: F,
) -> Result<Child>
where
    F: FnOnce(&mut Child) -> Result<()>,
{
    let directory = store.directory();
    // An exec Linux would refuse is refused here, before the launch phase moves.
    #[cfg(target_os = "linux")]
    crate::native::provider_process::linux_exec_budget(command)
        .context("provider launch was refused before spawn")?;
    let _lock = Store::open_unchecked(directory).lock()?;
    let mut record = read(store)?;
    // A command that carries a trust-bearing launch option is asked about here, under the lock
    // and immediately before the deadline check, so that the order (deadline, `Spawning`,
    // spawn) is unchanged: its consent, bound to the provider and workspace its plan was
    // granted for, must hold now. The question is bounded by the launch's remaining time, so it
    // cannot push the start past the receipt's expiry; an expired receipt is left to the
    // deadline check. A refusal leaves the launch pending: no provider was started. A change of
    // consent during the durable write that follows takes effect for the next act, not this
    // one. A command without such an option reads no consent, as 0.4.49 admitted it.
    if let Some(grant) = grant {
        match record
            .as_ref()
            .map(|record| record.remaining(Duration::ZERO))
        {
            Some(Some(remaining)) if remaining.is_zero() => {}
            Some(Some(remaining)) => {
                crate::native::consent::admit_launch(directory, grant, Some(remaining))?
            }
            _ => crate::native::consent::admit_launch(directory, grant, None)?,
        }
    }
    check_spawn_locked(store, record.as_ref())?;
    if let Some(record) = &mut record {
        record.phase = Phase::Spawning;
        Store::open_unchecked(directory)
            .record(CoreRecord::Launch)
            .write_json(record)?;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // Command::spawn returning Err establishes that it started no provider.
            if let Some(record) = &mut record {
                record.phase = Phase::Pending;
                Store::open_unchecked(directory)
                    .record(CoreRecord::Launch)
                    .write_json(record)?;
            }
            return Err(error).context("failed to spawn provider process");
        }
    };
    if let Err(error) = record_process(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        // The child existed and could already have consumed its argument prompt. Keep
        // Spawning even after cleanup; missing durable evidence is never a retry permit.
        return Err(error);
    }
    if let Some(record) = &mut record {
        record.phase = Phase::Spawned;
        // The authoritative process record is already durable. An auxiliary handshake
        // write must not kill a successfully started provider; readers also see its record.
        if let Err(error) = Store::open_unchecked(directory)
            .record(CoreRecord::Launch)
            .write_json(record)
        {
            log(
                store,
                &format!("launch receipt update failed after spawn: {error:#}"),
            );
        }
    }
    log(store, &format!("provider_spawned pid={}", child.id()));
    Ok(child)
}

/// The wrapper's admission: the launch it was started for is still the pending one.
pub(in crate::native) fn admit_wrapper(store: &Store) -> Result<()> {
    let directory = store.directory();
    let reader = Reader::open_unchecked(directory);
    if let Some(record) = read(&reader)? {
        // The wrapper has no deadline: the spawn fence judges it under the lifecycle lock.
        if pending::judge(
            &reader,
            &record,
            &record.claim_token,
            pending::Expiry::Unchecked,
        )?
        .is_err()
        {
            log(
                &Store::open_unchecked(directory),
                &format!("{WRAPPER_EXIT_CODE_MARKER}1; launch cancelled or already attempted"),
            );
            bail!(
                "provider launch cancelled or already attempted; the recorded session is unchanged"
            );
        }
    }
    Ok(())
}

/// Settles the session after the wrapper's command returned: recovers a pending completion,
/// records Exited/Failed, keeps the claim when the spawn is uncertain, otherwise releases it.
pub(in crate::native) fn finalize(store: &Store, result: &Result<()>) -> Result<()> {
    let directory = store.directory();
    let claim_path = Reader::open_unchecked(directory)
        .record(CoreRecord::TurnClaim)
        .path()
        .to_owned();
    let _claim_lock = Store::open_unchecked((claim_path).with_file_name("")).lock()?;
    if let Some(record) = read(&Reader::open_unchecked(directory))?
        && record.phase != Phase::Spawned
        && turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
            != Some(&record.claim_token)
    {
        return Ok(());
    }
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = Reader::open_unchecked(directory).status()?;
    if !status.state.ended() {
        match result {
            Ok(()) => update_status(directory, SessionState::Exited, Some(0), None)?,
            Err(error) => {
                let reason = if uncertain(&Reader::open_unchecked(directory)) {
                    format!(
                        "{error:#}; provider spawn is uncertain; the claim is retained, do not resend"
                    )
                } else {
                    format!("{error:#}")
                };
                update_status(directory, SessionState::Failed, Some(1), Some(reason))?;
            }
        }
    }
    if uncertain(&Reader::open_unchecked(directory)) && status.state != SessionState::Closed {
        return Ok(());
    }
    remove_turn_claim_locked(&claim_path)
}

fn uncertain(reader: &Reader) -> bool {
    read(reader)
        .ok()
        .flatten()
        .is_some_and(|record| record.phase == Phase::Spawning)
}

/// What [`diagnostic`] concludes about a launch that has not recorded its spawn. A closed
/// set: each renderer maps it to its own public vocabulary exhaustively.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Verdict {
    /// The session failed while the record shows a spawn in progress: a provider may exist.
    Uncertain,
    /// The session failed while the launch was pending: no provider process was started.
    Failed,
    /// The current claim's launch deadline expired while the session was still launching.
    Timeout,
}

/// Called with the read-only snapshot's lifecycle lock held.
pub(in crate::native) fn diagnostic(
    record: Option<&Record>,
    status: &SessionStatus,
    claim: Option<&str>,
) -> Option<(Verdict, String)> {
    let record = record?;
    if record.phase == Phase::Spawned {
        return None;
    }
    if status.state == SessionState::Failed {
        return Some((
            if record.phase == Phase::Spawning {
                Verdict::Uncertain
            } else {
                Verdict::Failed
            },
            status
                .error
                .clone()
                .unwrap_or_else(|| "provider launch failed".to_owned()),
        ));
    }
    if status.state == SessionState::Launching
        && claim == Some(&record.claim_token)
        && unix_ms() >= record.deadline_unix_ms
    {
        return Some((Verdict::Timeout, "provider launch deadline expired; run sessions for this workspace to settle startup, then inspect the same request; do not resend".to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use crate::native::session;
    use crate::native::session::names::*;
    use std::{fs, process::Stdio};

    fn fixture() -> (tempfile::TempDir, String) {
        let (directory, _, token) =
            test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
        (directory, token)
    }

    #[test]
    fn r1_c6_mismatch_fails_launch_before_deadline() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        assert!(
            repair_with_owner(&store, |_| Ok(Some(
                super::super::owner::tests::reused_attestation()
            )))
            .unwrap()
        );
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert!(
            store
                .status()
                .unwrap()
                .error
                .unwrap()
                .contains("wrapper exited")
        );
    }

    #[test]
    fn launch_repair_requires_process_death_not_projected_bools() {
        use crate::native::process_identity::{Attested, Gone, ProcessObservation};
        use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
        let mut failures = Vec::new();
        for pid_alive in [false, true] {
            for (case, process, control, dead, unknown) in [
                (
                    "unknown",
                    ProcessObservation::Unknown("inspection denied".into()),
                    ControlAttestation::NotRecorded,
                    false,
                    true,
                ),
                (
                    "live changed control",
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Changed("TTY changed".into()),
                    false,
                    false,
                ),
                (
                    "live unknown control",
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Unknown("control denied".into()),
                    false,
                    false,
                ),
                (
                    "live incarnation",
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::Verified,
                    false,
                    false,
                ),
                (
                    "exited",
                    ProcessObservation::Gone(Gone::Exited),
                    ControlAttestation::NotRecorded,
                    true,
                    false,
                ),
                (
                    "reused",
                    ProcessObservation::Gone(Gone::Reused("birth changed".into())),
                    ControlAttestation::NotRecorded,
                    true,
                    false,
                ),
                (
                    "legacy",
                    crate::native::process_identity::classify::<()>(
                        42,
                        None,
                        || pid_alive,
                        || panic!("pid-only record"),
                    ),
                    ControlAttestation::NotRecorded,
                    !pid_alive,
                    false,
                ),
            ] {
                let (directory, token) = fixture();
                let store = Store::open_unchecked(directory.path());
                let before = fs::read(directory.path().join(STATUS_FILE)).unwrap();
                let a = OwnerAttestation {
                    process,
                    control,
                    pid_alive: Some(pid_alive),
                };
                let result = repair_with_owner(&store, |_| Ok(Some(a)));
                let state = store.status().unwrap().state;
                let unchanged = fs::read(directory.path().join(STATUS_FILE)).unwrap() == before
                    && turn::current_claim_token(&store).unwrap().as_deref()
                        == Some(token.as_str());
                if result.is_err() != unknown
                    || (state == SessionState::Failed) != dead
                    || (!dead && !unchanged)
                {
                    failures.push(format!("{case}, raw alive={pid_alive}: result={result:?}, state={state}, unchanged={unchanged}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    fn child_command() -> Command {
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("cmd.exe");
            c.args(["/c", "pause"]);
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "read _"]);
            c
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn only_this_waits_own_recorded_deadline_carries_the_timeout_hint() {
        use crate::native::failure_kind::{FailureKind, classify};
        let surface: terminal::TerminalSession = serde_json::from_value(
            serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
        )
        .unwrap();
        // This wait's own deadline, recorded by its own failure write.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let error = wait(&store, &surface, Instant::now()).unwrap_err();
        assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
        assert_eq!(format!("{error:#?}"), format!("{LAUNCH_DEADLINE:?}"));
        assert_eq!(classify(&error), Some(FailureKind::TimedOut));
        assert_eq!(
            store.status().unwrap().error.as_deref(),
            Some(LAUNCH_DEADLINE)
        );
        // A spawn in progress records other text: it stays a plain message.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let mut record = read(&store).unwrap().unwrap();
        record.phase = Phase::Spawning;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
        let error = wait(&store, &surface, Instant::now()).unwrap_err();
        let recorded = store.status().unwrap().error.unwrap();
        assert_ne!(recorded, LAUNCH_DEADLINE);
        assert_eq!(format!("{error:#}"), recorded);
        assert_eq!(classify(&error), None);
        // The deadline while another holder keeps the lock: its own sentence, nothing written.
        let (directory, token) = fixture();
        let store = Store::open_unchecked(directory.path());
        let before = fs::read(directory.path().join(STATUS_FILE)).unwrap();
        let lock = store.lock().unwrap();
        let error = wait(&store, &surface, Instant::now()).unwrap_err();
        drop(lock);
        assert_eq!(
            format!("{error:#}"),
            format!(
                "{LAUNCH_DEADLINE}; startup is still holding the lifecycle lock, execution is uncertain; the claim is retained"
            )
        );
        assert_eq!(classify(&error), Some(FailureKind::TimedOut));
        assert_eq!(
            fs::read(directory.path().join(STATUS_FILE)).unwrap(),
            before
        );
        assert_eq!(
            turn::current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
    }

    // The wall clock is fixed, so the bound is bracketed by the monotonic readings around the
    // call: the recorded deadline plus the grace, never earlier than now, none past the clock.
    #[test]
    fn the_wrappers_recheck_bound_is_the_recorded_deadline_plus_the_grace() {
        struct Reset(Option<u128>);
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::native::session::tests::FIXED_UNIX_MS.set(self.0);
            }
        }
        const NOW: u128 = 1_000_000;
        let _reset = Reset(crate::native::session::tests::FIXED_UNIX_MS.replace(Some(NOW)));
        let bound = |deadline_unix_ms| {
            let record = Record {
                schema: 1,
                claim_token: "claim".to_owned(),
                deadline_unix_ms,
                phase: Phase::Pending,
            };
            let before = Instant::now();
            let bound = record.recheck_bound(Instant::now);
            (before, bound, Instant::now())
        };
        assert_eq!(RECHECK_GRACE, Duration::from_secs(2));
        for (deadline, remaining_ms) in [
            (NOW + 500, 2_500),
            // A deadline already past still leaves the rest of the grace.
            (NOW - 500, 1_500),
            (NOW - 2_000, 0),
            // A deadline older than the grace is now.
            (NOW - 5_000, 0),
            (0, 0),
        ] {
            let (before, bound, after) = bound(deadline);
            let bound = bound.unwrap();
            let remaining = Duration::from_millis(remaining_ms);
            assert!(
                before + remaining <= bound && bound <= after + remaining,
                "deadline {deadline}: {:?} after the call began",
                bound.saturating_duration_since(before)
            );
        }
        assert_eq!(bound(u128::MAX).1, None);
        // The largest deadline a launch record can be read with: a bound where the monotonic
        // clock can represent it, none where it cannot, and never a panic.
        let largest = u128::from(u64::MAX);
        let (before, bound, _) = bound(largest);
        if let Some(bound) = bound {
            let remaining = u64::try_from(largest + RECHECK_GRACE.as_millis() - NOW).unwrap();
            assert!(
                before
                    .checked_add(Duration::from_millis(remaining))
                    .is_some_and(|earliest| earliest <= bound)
            );
        }
    }

    // The re-check reads the wall clock before the monotonic clock, so that a pause between the
    // two readings can only lengthen its bound. A pause right after the monotonic reading
    // leaves the bound at the recorded deadline plus the grace; one right before it lengthens
    // the bound by the pause.
    #[test]
    fn the_wrappers_recheck_bound_reads_the_wall_clock_before_the_monotonic_clock() {
        use crate::native::session::tests::FIXED_UNIX_MS;
        struct Reset(Option<u128>);
        impl Drop for Reset {
            fn drop(&mut self) {
                FIXED_UNIX_MS.set(self.0);
            }
        }
        const NOW: u128 = 1_000_000;
        const PAUSE: Duration = Duration::from_secs(6);
        let _reset = Reset(FIXED_UNIX_MS.replace(Some(NOW)));
        let record = Record {
            schema: 1,
            claim_token: "claim".to_owned(),
            deadline_unix_ms: NOW + 10_000,
            phase: Phase::Pending,
        };
        let reading = Instant::now();
        let pause = || FIXED_UNIX_MS.set(Some(NOW + PAUSE.as_millis()));
        let after_reading = record.recheck_bound(|| {
            pause();
            reading
        });
        FIXED_UNIX_MS.set(Some(NOW));
        let before_reading = record.recheck_bound(|| {
            pause();
            reading + PAUSE
        });
        assert_eq!(
            (after_reading, before_reading),
            (
                Some(reading + Duration::from_secs(12)),
                Some(reading + PAUSE + Duration::from_secs(12))
            )
        );
    }

    // The wall-clock half of the receipt's conversion, and the saturating monotonic half that
    // the launch host and the binding wait use.
    #[test]
    fn the_receipt_deadline_converts_to_the_monotonic_clock_saturating() {
        struct Reset(Option<u128>);
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::native::session::tests::FIXED_UNIX_MS.set(self.0);
            }
        }
        const NOW: u128 = 1_000_000;
        let _reset = Reset(crate::native::session::tests::FIXED_UNIX_MS.replace(Some(NOW)));
        let ms = Duration::from_millis;
        let remaining = |deadline_unix_ms, after| {
            Record {
                schema: 1,
                claim_token: "claim".to_owned(),
                deadline_unix_ms,
                phase: Phase::Pending,
            }
            .remaining(after)
        };
        assert_eq!(remaining(NOW + 500, Duration::ZERO), Some(ms(500)));
        assert_eq!(remaining(NOW, Duration::ZERO), Some(Duration::ZERO));
        assert_eq!(remaining(NOW - 500, Duration::ZERO), Some(Duration::ZERO));
        assert_eq!(remaining(0, Duration::ZERO), Some(Duration::ZERO));
        // A time after the deadline counts from the deadline, even once it has passed.
        assert_eq!(remaining(NOW - 500, ms(2_000)), Some(ms(1_500)));
        assert_eq!(remaining(NOW - 5_000, ms(2_000)), Some(Duration::ZERO));
        let largest = u128::from(u64::MAX);
        assert_eq!(remaining(largest + NOW, Duration::ZERO), Some(ms(u64::MAX)));
        assert_eq!(remaining(largest + NOW + 1, Duration::ZERO), None);
        assert_eq!(remaining(u128::MAX, Duration::ZERO), None);

        let now = Instant::now();
        let furthest = |instant: Instant| instant.checked_add(Duration::from_nanos(1)).is_none();
        assert_eq!(saturating_deadline(now, Some(ms(500))), now + ms(500));
        assert_eq!(saturating_deadline(now, Some(Duration::ZERO)), now);
        // Too long for the conversion: `u64::MAX` milliseconds, or the clock's furthest instant.
        let saturated = saturating_deadline(now, None);
        match now.checked_add(ms(u64::MAX)) {
            Some(expected) => assert_eq!(saturated, expected),
            None => assert!(furthest(saturated)),
        }
        // Past what the monotonic clock represents: its furthest instant, never a panic.
        let saturated = saturating_deadline(now, Some(Duration::MAX));
        assert!(saturated > now && furthest(saturated));
    }

    #[test]
    fn launch_repair_missing_unreadable_owner_and_deadline_are_distinct() {
        let (directory, token) = fixture();
        let store = Store::open_unchecked(directory.path());
        let before = fs::read(directory.path().join(STATUS_FILE)).unwrap();
        assert!(
            repair(&store).unwrap(),
            "a wrapper may not have registered yet"
        );
        assert_eq!(
            fs::read(directory.path().join(STATUS_FILE)).unwrap(),
            before
        );
        store
            .record(CoreRecord::Owner)
            .write_json(&"broken")
            .unwrap();
        assert!(repair(&store).is_err());
        assert_eq!(
            fs::read(directory.path().join(STATUS_FILE)).unwrap(),
            before
        );
        assert_eq!(
            turn::current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
        let mut record = read(&store).unwrap().unwrap();
        record.deadline_unix_ms = 0;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
        assert!(
            repair(&store).unwrap(),
            "timeout does not need owner evidence"
        );
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert!(status.error.unwrap().contains("launch timed out"));
        assert!(turn::current_claim_token(&store).unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn launch_repair_observes_an_exited_windows_incarnation() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let mut child = child_command().spawn().unwrap();
        let pid = child.id();
        let identity = terminal::observe_windows_process_identity(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        store
            .write_owner(&crate::native::terminal::ownership::NativeSessionOwner {
                pid,
                windows_process_identity: Some(identity.unwrap().expect("live child identity")),
                ..Default::default()
            })
            .unwrap();
        assert!(repair(&store).unwrap());
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert!(
            store
                .status()
                .unwrap()
                .error
                .unwrap()
                .contains("wrapper exited")
        );
        assert!(turn::current_claim_token(&store).unwrap().is_none());
    }

    #[test]
    fn unproven_terminal_failure_records_no_handle_and_no_prompt() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        terminal_failed(&store, &anyhow::anyhow!("iTerm2 automation timed out")).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some(
                "terminal launch failed: iTerm2 automation timed out; no surface handle was recorded and no prompt was sent"
            )
        );
        assert_eq!(status.residual_surface(), None);
        assert!(store.terminal().is_err());
    }

    #[test]
    fn bound_surface_or_spawn_in_progress_keeps_its_own_failure_wording() {
        // A surface bound before the failure keeps its handle, so the sentence is absent.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let surface: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
        }))
        .unwrap();
        store.write_terminal(&surface).unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(
            status.error.as_deref(),
            Some("terminal launch failed: Ghostty start failed")
        );
        assert!(store.terminal().is_ok());

        // A handle that a close moved to the closing record is still a handle.
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        store
            .record(CoreRecord::TerminalClosing)
            .write_json(&surface)
            .unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        assert_eq!(
            store.status().unwrap().error.as_deref(),
            Some("terminal launch failed: Ghostty start failed")
        );
        assert!(
            store
                .record(CoreRecord::TerminalClosing)
                .text()
                .unwrap()
                .is_some()
        );

        // A spawn in progress may have carried the prompt: the spawn wording stays alone.
        let (directory, token) = fixture();
        let store = Store::open_unchecked(directory.path());
        let mut record = read(&store).unwrap().unwrap();
        assert_eq!(record.claim_token, token);
        record.phase = Phase::Spawning;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
        terminal_failed(&store, &anyhow::anyhow!("Ghostty start failed")).unwrap();
        let error = store.status().unwrap().error.unwrap();
        assert!(error.contains("provider spawn is uncertain"), "{error}");
        assert!(!error.contains("no prompt was sent"), "{error}");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn retained_handoff_marks_stored_and_returned_diagnostics_once() {
        for case in ["normal", "lost", "closed", "write", "read"] {
            let (directory, _) = fixture();
            let store = Store::open_unchecked(directory.path());
            if matches!(case, "lost" | "closed") {
                fail(&store, "launch cancelled").unwrap();
                if case == "closed" {
                    session::close::close(
                        &store,
                        None,
                        crate::native::session::close::budget::close_deadline(),
                        |_| panic!("unbound"),
                    )
                    .unwrap();
                }
            }
            if case == "write" {
                fs::create_dir(store.record(CoreRecord::Terminal).path()).unwrap();
            }
            if case == "read" {
                fs::write(store.record(CoreRecord::Launch).path(), "broken").unwrap();
            }
            let surface = serde_json::from_value(serde_json::json!({
                "terminal":"warp", "session_id":"instance-1", "tab_id":"tab-1", "window_id":"window-1"
            })).unwrap();
            let error =
                terminal::RetainedLaunchSurface::new(surface, "exact cleanup failed".into())
                    .with_unverified_cleanup()
                    .into();
            let before = store.status().unwrap();
            let returned = terminal_failure(&store, error);
            let text = format!("{returned:#}");
            assert_eq!(
                text.matches(RESIDUAL_SURFACE_MARKER).count(),
                1,
                "{case}: {text}"
            );
            let json = serde_json::json!({"error": text});
            assert_eq!(
                json["error"]
                    .as_str()
                    .unwrap()
                    .matches(RESIDUAL_SURFACE_MARKER)
                    .count(),
                1
            );
            let status = store.status().unwrap();
            if case == "read" {
                // A malformed launch record cannot grant a status transition.
                assert_eq!(status.state, SessionState::Launching);
                assert_eq!(status.generation, before.generation);
                assert_eq!(status.updated_unix_ms, before.updated_unix_ms);
                assert_eq!(
                    status
                        .error
                        .as_deref()
                        .unwrap()
                        .matches(RESIDUAL_SURFACE_MARKER)
                        .count(),
                    1
                );
                assert_eq!(status.residual_surface(), Some(ResidualSurface::Unverified));
            } else {
                assert_eq!(
                    status
                        .error
                        .as_deref()
                        .unwrap()
                        .matches(RESIDUAL_SURFACE_MARKER)
                        .count(),
                    1,
                    "{case}"
                );
                assert_eq!(status.residual_surface(), Some(ResidualSurface::Unverified));
                if matches!(case, "normal" | "write") {
                    assert_eq!(status.error.as_deref(), Some(format!("terminal launch failed: exact cleanup failed; {RESIDUAL_SURFACE_MARKER}").as_str()));
                }
                if matches!(case, "lost" | "closed") {
                    assert_eq!(status.generation, before.generation);
                    assert_eq!(status.updated_unix_ms, before.updated_unix_ms);
                }
                if case == "closed" {
                    assert_eq!(
                        store.closed_if_present().unwrap().unwrap().error,
                        status.error
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn retained_surface_record_failure_is_durable() {
        let (directory, _) = fixture();
        let store = Store::open_unchecked(directory.path());
        let surface = serde_json::from_value(serde_json::json!({
            "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
        }))
        .unwrap();
        let error =
            terminal::RetainedLaunchSurface::new(surface, "exact cleanup failed".to_owned()).into();
        let handle = store.record(CoreRecord::Terminal).path().to_owned();
        fs::create_dir(&handle).unwrap();
        assert!(terminal_failed(&store, &error).is_err());
        fs::remove_dir(&handle).unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Failed);
        assert_eq!(
            store.status().unwrap().residual_surface(),
            Some(ResidualSurface::Unverified)
        );
        session::close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |_| panic!("no persisted handle grants authority"),
        )
        .unwrap();
        assert_eq!(
            store.status().unwrap().residual_surface(),
            Some(ResidualSurface::Unverified)
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn unbound_residual_survives_changed_diagnostics_and_repeated_close() {
        for closed_first in [false, true] {
            let (directory, _) = fixture();
            let store = Store::open_unchecked(directory.path());
            fail(&store, "launch cancelled").unwrap();
            if closed_first {
                session::close::close(
                    &store,
                    None,
                    crate::native::session::close::budget::close_deadline(),
                    |_| panic!("no surface is bound"),
                )
                .unwrap();
            }
            let surface = serde_json::from_value(serde_json::json!({
                "terminal":"ghostty", "session_id":"u2", "tab_id":"t2", "window_id":"w1"
            }))
            .unwrap();
            let error =
                terminal::RetainedLaunchSurface::new(surface, "late failure".to_owned()).into();
            assert!(terminal_failed(&store, &error).is_err());
            let mut status = store.status().unwrap();
            assert_eq!(status.residual_surface, Some(ResidualSurface::Unverified));
            status.error = Some("new diagnostic wording".to_owned());
            store.write_status(&status).unwrap();
            if closed_first {
                store.write_closed(&status).unwrap();
                // Simulate interruption after the tombstone amendment but before the
                // status amendment. Close convergence must restore the typed fact.
                status.residual_surface = None;
                store.write_status(&status).unwrap();
                store.converge().unwrap();
                assert_eq!(
                    store.status().unwrap().residual_surface,
                    Some(ResidualSurface::Unverified)
                );
            }
            for _ in 0..2 {
                session::close::close(
                    &store,
                    None,
                    crate::native::session::close::budget::close_deadline(),
                    |_| panic!("residual evidence grants no authority"),
                )
                .unwrap();
                let status = store.status().unwrap();
                assert_eq!(status.state, SessionState::Closed);
                assert_eq!(status.residual_surface(), Some(ResidualSurface::Unverified));
                assert_eq!(status.error.as_deref(), Some("new diagnostic wording"));
                assert_eq!(
                    store.closed_if_present().unwrap().unwrap().residual_surface,
                    Some(ResidualSurface::Unverified)
                );
                assert!(!store.record(CoreRecord::Terminal).path().exists());
            }
        }
    }

    #[test]
    fn diagnostic_yields_each_verdict_only_for_an_unspawned_launch() {
        let status = |state, error: Option<&str>| SessionStatus {
            state,
            generation: 1,
            updated_unix_ms: 1,
            exit_code: None,
            error: error.map(str::to_owned),
            residual_surface: None,
        };
        let record = |phase, deadline_unix_ms| Record {
            schema: 1,
            claim_token: "claim".to_owned(),
            deadline_unix_ms,
            phase,
        };
        let failed = status(SessionState::Failed, Some("spawn broke"));
        assert_eq!(
            diagnostic(Some(&record(Phase::Spawning, u128::MAX)), &failed, None),
            Some((Verdict::Uncertain, "spawn broke".to_owned()))
        );
        assert_eq!(
            diagnostic(
                Some(&record(Phase::Pending, u128::MAX)),
                &status(SessionState::Failed, None),
                None
            ),
            Some((Verdict::Failed, "provider launch failed".to_owned()))
        );
        let launching = status(SessionState::Launching, None);
        let expired = record(Phase::Pending, 1);
        let (verdict, detail) = diagnostic(Some(&expired), &launching, Some("claim")).unwrap();
        assert_eq!(verdict, Verdict::Timeout);
        assert!(detail.starts_with("provider launch deadline expired;"));
        // Another claim, a live deadline, a recorded spawn or no record yields none.
        assert_eq!(diagnostic(Some(&expired), &launching, Some("other")), None);
        assert_eq!(
            diagnostic(
                Some(&record(Phase::Spawning, u128::MAX)),
                &launching,
                Some("claim")
            ),
            None
        );
        assert_eq!(
            diagnostic(Some(&record(Phase::Spawned, 1)), &failed, Some("claim")),
            None
        );
        assert_eq!(diagnostic(None, &failed, None), None);
    }

    #[test]
    fn cancelled_launch_cannot_spawn_late() {
        let (directory, _) = fixture();
        fail(&Store::open_unchecked(directory.path()), "launch timed out").unwrap();
        let error = spawn(
            &Store::open_unchecked(directory.path()),
            &mut child_command(),
            |_| panic!("cancelled provider spawned"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!directory.path().join(PROVIDER_PROCESS_FILE).exists());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn long_bootstrap_is_sourced_from_a_private_file_through_a_short_command() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let script = format!("# {}\nprintf 'SCRIPT_OK\\n'\nexit 3", "x".repeat(8192));
        let command = install_script(&Store::open_unchecked(directory.path()), &script).unwrap();
        assert!(command.len() < 512);
        assert_eq!(
            fs::read_to_string(directory.path().join("launch.sh")).unwrap(),
            script
        );
        assert_eq!(
            fs::metadata(directory.path().join("launch.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "SCRIPT_OK\n");
    }
}

#[cfg(test)]
mod host_decision_tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Store, String) {
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT)
    }
    fn failed_host(store: &Store, token: &str) {
        host::initialize(store, token, terminal::TerminalKind::Iterm2).unwrap();
        let identity = host::HostIdentity {
            pid: 123,
            start_seconds: 123,
            start_microseconds: 1,
        };
        host::admit(store, token, identity).unwrap();
        host::advance(
            store,
            token,
            identity,
            host::Stage::EnvironmentPreparing,
            host::Progress {
                capture_budget_ms: 10000,
                ..Default::default()
            },
        )
        .unwrap();
        host::fail(
            store,
            token,
            Some(identity),
            host::Failure {
                reason: host::FailureReason::CaptureTimeout,
                exit_code: None,
                signal: None,
            },
            10000,
            Some(10000),
            None,
        )
        .unwrap();
    }
    fn surface() -> terminal::TerminalSession {
        terminal::TerminalSession::new(terminal::TerminalKind::Iterm2, "unused".into())
    }
    #[test]
    fn wait_and_repair_fail_current_host_before_deadline_with_the_same_reason() {
        let mut errors = Vec::new();
        for repair in [false, true] {
            let (_dir, store, token) = fixture();
            failed_host(&store, &token);
            let start = Instant::now();
            if repair {
                assert!(
                    repair_with_owner(&store, |_| panic!(
                        "host failure precedes owner observation"
                    ))
                    .unwrap()
                );
            } else {
                let error = wait(&store, &surface(), start + Duration::from_secs(20)).unwrap_err();
                assert!(error.downcast_ref::<TimedOut>().is_none());
            }
            assert!(start.elapsed() < Duration::from_secs(1));
            assert_eq!(store.status().unwrap().state, SessionState::Failed);
            assert!(turn::current_claim_token(&store).unwrap().is_none());
            errors.push(store.status().unwrap().error.unwrap());
        }
        assert_eq!(errors[0], errors[1]);
        assert!(errors[0].contains(
            "10000 ms (capture_timeout); last recorded launch stage: environment_preparing"
        ));
    }
    #[test]
    fn non_capture_failure_names_the_host_instead_of_environment_preparation() {
        let (_dir, store, token) = fixture();
        host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
        host::fail(
            &store,
            &token,
            None,
            host::Failure {
                reason: host::FailureReason::IdentityUnavailable,
                exit_code: None,
                signal: None,
            },
            0,
            None,
            None,
        )
        .unwrap();
        assert!(
            repair_with_owner(&store, |_| panic!(
                "host failure precedes owner observation"
            ))
            .unwrap()
        );
        assert_eq!(
            store.status().unwrap().error.as_deref(),
            Some(
                "terminal launch host failed (identity_unavailable); last recorded launch stage: awaiting_host"
            )
        );
    }
    #[test]
    fn stale_late_and_confirmed_records_never_override_current_state() {
        for case in ["claim", "receipt", "ended", "closed", "confirmed"] {
            let (_dir, store, token) = fixture();
            failed_host(&store, &token);
            match case {
                "claim" => {
                    std::fs::write(store.record(CoreRecord::TurnClaim).path(), "successor").unwrap()
                }
                "receipt" | "confirmed" => {
                    let mut receipt = read(&store).unwrap().unwrap();
                    if case == "receipt" {
                        receipt.claim_token = "successor".into();
                    } else {
                        receipt.phase = Phase::Spawned;
                    }
                    store
                        .record(CoreRecord::Launch)
                        .write_json(&receipt)
                        .unwrap();
                }
                "ended" => update_status(
                    store.directory(),
                    SessionState::Failed,
                    None,
                    Some("earlier failure".into()),
                )
                .unwrap(),
                "closed" => store.write_closed(&store.status().unwrap()).unwrap(),
                _ => unreachable!(),
            }
            let before = store.status().unwrap();
            let _lock = store.lock().unwrap();
            assert_ne!(
                host_decision_locked(&store).unwrap(),
                HostDecision::Failed,
                "{case}"
            );
            assert_eq!(store.status().unwrap().error, before.error);
            assert_eq!(store.status().unwrap().state, before.state);
        }
    }
    #[test]
    fn uncertain_spawn_keeps_claim_and_deadline_suffix_preserves_legacy_text() {
        let (_dir, store, token) = fixture();
        assert_eq!(
            deadline_reason(&store, read(&store).unwrap().as_ref()),
            LAUNCH_DEADLINE
        );
        failed_host(&store, &token);
        assert_eq!(
            deadline_reason(&store, read(&store).unwrap().as_ref()),
            format!("{LAUNCH_DEADLINE}; last recorded launch stage: environment_preparing")
        );
        let mut receipt = read(&store).unwrap().unwrap();
        receipt.phase = Phase::Spawning;
        store
            .record(CoreRecord::Launch)
            .write_json(&receipt)
            .unwrap();
        let _lock = store.lock().unwrap();
        assert_eq!(host_decision_locked(&store).unwrap(), HostDecision::Failed);
        assert_eq!(
            turn::current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
        assert!(
            store
                .status()
                .unwrap()
                .error
                .unwrap()
                .contains("provider spawn is uncertain")
        );
    }
    #[test]
    fn deadline_wait_keeps_timeout_hint_only_for_its_own_exact_reason() {
        for recorded in [false, true] {
            let (_dir, store, token) = fixture();
            if recorded {
                host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
            }
            let expected = if recorded {
                format!("{LAUNCH_DEADLINE}; last recorded launch stage: awaiting_host")
            } else {
                LAUNCH_DEADLINE.into()
            };
            let error = wait(&store, &surface(), Instant::now()).unwrap_err();
            assert!(error.downcast_ref::<TimedOut>().is_some());
            assert_eq!(error.to_string(), expected);
            assert_eq!(
                store.status().unwrap().error.as_deref(),
                Some(expected.as_str())
            );
        }
    }

    /// Records `failure` for a current host at `stage`, the stage at which the launch host
    /// refuses for that reason; every capture budget is 9996 ms.
    fn host_failed_at(store: &Store, token: &str, stage: host::Stage, failure: host::Failure) {
        host::initialize(store, token, terminal::TerminalKind::Iterm2).unwrap();
        let identity = host::HostIdentity {
            pid: 123,
            start_seconds: 123,
            start_microseconds: 1,
        };
        let identity = (stage != host::Stage::AwaitingHost).then_some(identity);
        if let Some(identity) = identity {
            host::admit(store, token, identity).unwrap();
            for next in [
                host::Stage::EnvironmentPreparing,
                host::Stage::EnvironmentReady,
                host::Stage::LaunchShellStarting,
            ] {
                if next > stage {
                    break;
                }
                host::advance(
                    store,
                    token,
                    identity,
                    next,
                    host::Progress {
                        capture_budget_ms: 9996,
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
        host::fail(store, token, identity, failure, 10_000, None, None).unwrap();
    }

    // status.error of a current host failure, for every reason, as the repair and the wait
    // record it: the exit status and signal never appear in it.
    #[test]
    fn host_failure_status_error_for_every_reason() {
        use host::FailureReason::*;
        use host::Stage::*;
        let cases = [
            (IdentityUnavailable, AwaitingHost, None, None),
            (BindingMismatch, EnvironmentPreparing, None, None),
            (BindingTimeout, EnvironmentPreparing, None, None),
            (LaunchBudgetExhausted, EnvironmentPreparing, None, None),
            (CaptureSpawnFailed, EnvironmentPreparing, None, None),
            (CaptureTimeout, EnvironmentPreparing, None, None),
            (CaptureExitNonzero, EnvironmentPreparing, Some(3), None),
            (CaptureSignaled, EnvironmentPreparing, None, Some(9)),
            (CaptureNoOutput, EnvironmentPreparing, None, None),
            (CaptureOversized, EnvironmentPreparing, None, None),
            (CaptureInvalid, EnvironmentPreparing, None, None),
            (CleanupFailed, EnvironmentPreparing, None, None),
            (InputFlushFailed, EnvironmentReady, None, None),
            (LaunchShellExecFailed, LaunchShellStarting, None, None),
        ];
        // Adding a reason fails to compile here until the table above names it.
        for (reason, ..) in cases {
            match reason {
                IdentityUnavailable
                | BindingMismatch
                | BindingTimeout
                | LaunchBudgetExhausted
                | CaptureSpawnFailed
                | CaptureTimeout
                | CaptureExitNonzero
                | CaptureSignaled
                | CaptureNoOutput
                | CaptureOversized
                | CaptureInvalid
                | CleanupFailed
                | InputFlushFailed
                | LaunchShellExecFailed => {}
            }
        }
        let mut actual = Vec::new();
        for (reason, stage, exit_code, signal) in cases {
            let (_dir, store, token) = fixture();
            host_failed_at(
                &store,
                &token,
                stage,
                host::Failure {
                    reason,
                    exit_code,
                    signal,
                },
            );
            assert!(
                repair_with_owner(&store, |_| panic!(
                    "host failure precedes owner observation"
                ))
                .unwrap()
            );
            assert_eq!(store.status().unwrap().state, SessionState::Failed);
            actual.push(store.status().unwrap().error.unwrap());
        }
        let expected = [
            "terminal launch host failed (identity_unavailable); last recorded launch stage: awaiting_host",
            "terminal launch host failed (binding_mismatch); last recorded launch stage: environment_preparing",
            "terminal launch host failed (binding_timeout); last recorded launch stage: environment_preparing",
            "terminal launch host failed (launch_budget_exhausted); last recorded launch stage: environment_preparing",
            "terminal launch host failed (capture_spawn_failed); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared: startup files did not finish within 9996 ms (capture_timeout); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared (capture_exit_nonzero); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared (capture_signaled); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared (capture_no_output); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared (capture_oversized); last recorded launch stage: environment_preparing",
            "terminal shell environment could not be prepared (capture_invalid); last recorded launch stage: environment_preparing",
            "terminal launch host failed (cleanup_failed); last recorded launch stage: environment_preparing",
            "terminal launch host failed (input_flush_failed); last recorded launch stage: environment_ready",
            "terminal launch host failed (launch_shell_exec_failed); last recorded launch stage: launch_shell_starting",
        ];
        assert_eq!(actual, expected);
    }
}
