//! What an Observation concludes from its Records and, on request, from the OS. Every
//! renderer maps these typed judgments to its own vocabulary and derives none of its own.
use super::{CancelObservation, SessionEvidence, SurfaceRecord};
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use crate::native::query::request::RequestState;
use crate::native::reopen::ResumedFrom;
use crate::native::session::ResidualSurface;
use crate::native::session::owner::{ControlAttestation, OwnerAttestation, attest_owner};
use crate::native::session::snapshot::Snapshot;
use crate::native::session::{self, OwnerObservation, SessionState, SessionStatus};
use crate::native::terminal::TerminalKind;
use crate::native::terminal::ownership::NativeSessionOwner;
use agent_bridge::process_is_alive;
use anyhow::Result;
use serde_json::Value;

pub(in crate::native) const FOREIGN_OWNER: &str = "owner record belongs to another session";

/// The Owner record judged against the session it was read for (CONTEXT.md Attestation).
/// Only a record bound to this session is attested.
#[derive(Debug)]
pub(in crate::native) enum OwnerJudgment {
    /// The record names this session; its incarnation and control identity were attested.
    Bound(OwnerAttestation),
    /// A legacy record without a binding: its PID is observed alive or gone, never attested.
    UnboundLegacy {
        alive: Option<bool>,
    },
    /// The record names another managed session and was not observed at all.
    Foreign,
    Absent,
    Unreadable(String),
}

/// What a Bound Owner's attestation decides. Renderers conclude a death from this,
/// never from the projected bools: an Unknown attestation is never a death (D15).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum BoundOwner {
    /// A live incarnation whose recorded control identity matched.
    #[cfg_attr(not(any(target_os = "macos", windows, test)), allow(dead_code))]
    Verified,
    /// A live incarnation whose control identity changed; not a death.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    ControlChanged,
    /// Alive, with no control identity to compare: a PID-only record, or a recorded
    /// birth without TTY and process groups.
    Uncompared,
    /// The recorded incarnation exited.
    Exited,
    /// The PID now belongs to another incarnation: the recorded one is gone.
    Reused,
    /// The incarnation or its control identity could not be inspected.
    Unknown,
}

/// The OS observations an owner judgment may make. Tests replace them.
pub(in crate::native) struct Observers<'a> {
    pub(in crate::native) attest: &'a dyn Fn(&NativeSessionOwner) -> OwnerAttestation,
    pub(in crate::native) pid_alive: &'a dyn Fn(u32) -> Option<bool>,
}

impl Observers<'static> {
    pub(in crate::native) const SYSTEM: Self = Self {
        attest: &attest_owner,
        pid_alive: &system_pid_alive,
    };
}

fn system_pid_alive(pid: u32) -> Option<bool> {
    Some(process_is_alive(pid))
}

/// Judges the raw Owner read for `session`. Reads no record and takes no lock.
pub(in crate::native) fn owner(
    record: &Result<Option<NativeSessionOwner>>,
    session: &str,
    observers: &Observers<'_>,
) -> OwnerJudgment {
    match record {
        Err(error) => OwnerJudgment::Unreadable(format!("{error:#}")),
        Ok(None) => OwnerJudgment::Absent,
        Ok(Some(owner)) => match owner.managed_session_id.as_deref() {
            Some(bound) if bound == session => OwnerJudgment::Bound((observers.attest)(owner)),
            Some(_) => OwnerJudgment::Foreign,
            None => OwnerJudgment::UnboundLegacy {
                alive: (observers.pid_alive)(owner.pid),
            },
        },
    }
}

impl BoundOwner {
    pub(in crate::native) fn of(attestation: &OwnerAttestation) -> Self {
        match &attestation.process {
            ProcessObservation::Live(Attested::Incarnation) => match &attestation.control {
                #[cfg(any(target_os = "macos", windows, test))]
                ControlAttestation::Verified => Self::Verified,
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Changed(_) => Self::ControlChanged,
                #[cfg(any(target_os = "macos", test))]
                ControlAttestation::Unknown(_) => Self::Unknown,
                ControlAttestation::NotRecorded => Self::Uncompared,
            },
            ProcessObservation::Live(Attested::PidOnly) => Self::Uncompared,
            ProcessObservation::Gone(Gone::Exited) => Self::Exited,
            ProcessObservation::Gone(Gone::Reused(_)) => Self::Reused,
            ProcessObservation::Unknown(_) => Self::Unknown,
        }
    }
}

impl OwnerJudgment {
    /// The Bound Owner's verdict; None for every record not bound to this session.
    pub(in crate::native) fn bound(&self) -> Option<BoundOwner> {
        match self {
            Self::Bound(attestation) => Some(BoundOwner::of(attestation)),
            _ => None,
        }
    }

    /// The recorded owner is verified gone: a Bound incarnation that exited or whose PID was
    /// reused, or an unbound legacy PID observed absent.
    pub(in crate::native) fn gone(&self) -> bool {
        match self {
            Self::Bound(attestation) => matches!(
                BoundOwner::of(attestation),
                BoundOwner::Exited | BoundOwner::Reused
            ),
            Self::UnboundLegacy { alive } => *alive == Some(false),
            Self::Foreign | Self::Absent | Self::Unreadable(_) => false,
        }
    }

    /// The renderer-side JSON shape of the judgment.
    pub(in crate::native) fn observation(&self) -> OwnerObservation {
        match self {
            Self::Bound(attestation) => OwnerObservation::from(attestation),
            Self::UnboundLegacy { alive } => OwnerObservation {
                process_alive: *alive,
                ..OwnerObservation::default()
            },
            Self::Foreign => OwnerObservation {
                error: Some(FOREIGN_OWNER.to_owned()),
                ..OwnerObservation::default()
            },
            Self::Absent => OwnerObservation::default(),
            Self::Unreadable(reason) => OwnerObservation {
                error: Some(reason.clone()),
                ..OwnerObservation::default()
            },
        }
    }
}

/// What the Snapshot says about the current turn. The launch diagnostic is computed by
/// the caller under the retained Snapshot lock (D20) and folded in here as the override
/// every renderer reports in place of the turn: it exists only for a launching or failed
/// session, and delivery is unconfirmed only for a working one, so the two never meet.
// The variants carry doctor's documented turn vocabulary.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Turn {
    LaunchFailed {
        reason: &'static str,
        detail: String,
    },
    /// A turn is claimed with a recorded error while the session works and no completion
    /// is journaled: delivery may have occurred, so nothing may be resent.
    DeliveryUnconfirmed,
    TurnInProgress,
    NoActiveTurn,
}

pub(in crate::native) fn turn(
    records: &Snapshot,
    launch_failure: Option<(&'static str, String)>,
) -> Turn {
    if let Some((reason, detail)) = launch_failure {
        return Turn::LaunchFailed { reason, detail };
    }
    match (&records.claim, &records.status.error) {
        (Some(_), Some(_))
            if records.status.state == SessionState::Working && records.pending.is_none() =>
        {
            Turn::DeliveryUnconfirmed
        }
        (Some(_), _) => Turn::TurnInProgress,
        (None, _) => Turn::NoActiveTurn,
    }
}

/// A Claim whose Receipt cannot be read: the turn's public request identity is unknown.
pub(in crate::native) fn claim_without_receipt(records: &Snapshot) -> bool {
    records.claim.is_some() && records.active_receipt().is_none()
}

/// A completion journal is pending: recovery, which no query performs, is required.
pub(in crate::native) fn recovery_required(records: &Snapshot) -> bool {
    records.pending.is_some()
}

/// The request index could not be read in full: absence proves no request absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) struct IndexIncomplete {
    pub(in crate::native) unreadable: usize,
    pub(in crate::native) error: Option<String>,
}

pub(in crate::native) fn request_index(records: &Snapshot) -> Option<IndexIncomplete> {
    (records.unreadable_requests > 0 || records.request_index_error.is_some()).then(|| {
        IndexIncomplete {
            unreadable: records.unreadable_requests,
            error: records.request_index_error.clone(),
        }
    })
}

/// A surface a failed launch may have left, as the status record observes it; a cleared
/// one is no residual surface.
pub(in crate::native) fn residual_surface(records: &Snapshot) -> Option<ResidualSurface> {
    records.status.residual_surface()
}

/// The stored lifecycle verdict, independent of owner and surface observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Lifecycle {
    Ready,
    Closed,
    Failed,
    Exited,
    Other,
}

pub(in crate::native) fn lifecycle(records: &Snapshot) -> Lifecycle {
    match records.status.state {
        SessionState::Ready => Lifecycle::Ready,
        SessionState::Closed => Lifecycle::Closed,
        SessionState::Failed => Lifecycle::Failed,
        SessionState::Exited => Lifecycle::Exited,
        _ => Lifecycle::Other,
    }
}

/// The user's follow-up Hold, with the whole release gate: a release is suggested only
/// while no close tombstone exists and the stored state is known and not closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Hold {
    Held { release_allowed: bool },
    NotHeld,
    Unreadable(String),
}

/// `close_tombstone` is the session close tombstone read for a held session (None when
/// it was not read); `stored_state` is None when no Snapshot could be read.
pub(in crate::native) fn hold(
    held: &Result<bool>,
    close_tombstone: Option<&Result<Option<SessionStatus>>>,
    stored_state: Option<&SessionState>,
) -> Hold {
    match held {
        Ok(false) => Hold::NotHeld,
        Err(error) => Hold::Unreadable(format!("{error:#}")),
        Ok(true) => Hold::Held {
            release_allowed: matches!(close_tombstone, Some(Ok(None)))
                && stored_state.is_some_and(|state| *state != SessionState::Closed),
        },
    }
}

/// A recorded cancel judged against its request's Result and the current Claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum CancelState {
    /// Requested for the turn that is still claimed.
    RequestedActive,
    /// Requested for a turn that is no longer the current Claim and has no Result.
    RequestedStale,
    Applied,
    NotApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Cancel {
    Absent,
    /// A cancel record exists; None when no Snapshot could resolve its request.
    Recorded(Option<CancelState>),
    Unreadable(String),
}

pub(in crate::native) fn cancel(observed: Option<&CancelObservation>) -> Cancel {
    observed.map_or(Cancel::Absent, |observed| observed.judgment.clone())
}

/// The cancel judgment of doctor's no-Snapshot fallback: the record alone, unresolved.
pub(in crate::native) fn cancel_record(record: &Result<Option<session::cancel::Cancel>>) -> Cancel {
    match record {
        Ok(None) => Cancel::Absent,
        Ok(Some(_)) => Cancel::Recorded(None),
        Err(error) => Cancel::Unreadable(format!("{error:#}")),
    }
}

/// Which surface record the Observation read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum SurfaceSource {
    ClosedTombstone,
    ClosingHandle,
    ActiveHandle,
}

/// The surface record judged for this session: the facts a renderer reports about it,
/// without reading the record again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Surface {
    /// A close tombstone; a consumed handle cannot be reused.
    Closed { consumed: bool },
    /// A claimed close handle, bound to this session or not.
    Closing { bound: bool },
    /// An active handle. Platform support is judged before the binding, and a legacy
    /// iTerm2 handle without a binding counts as bound (`verify_managed_session`).
    Active {
        kind: TerminalKind,
        supported: bool,
        bound: bool,
        empty_id: bool,
    },
    /// No surface record of any kind.
    Missing,
    Unreadable {
        source: SurfaceSource,
        reason: String,
    },
}

/// The active handle's verdict, in the judgment's precedence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum ActiveHandle {
    Unsupported,
    BindingUnverified,
    Bound,
}

impl ActiveHandle {
    /// Platform support is decided before the binding; an empty id is no binding.
    pub(in crate::native) fn of(supported: bool, bound: bool, empty_id: bool) -> Self {
        if !supported {
            Self::Unsupported
        } else if !bound || empty_id {
            Self::BindingUnverified
        } else {
            Self::Bound
        }
    }
}

pub(in crate::native) fn surface(record: &SurfaceRecord, session: &str) -> Surface {
    let unreadable = |source, error: &anyhow::Error| Surface::Unreadable {
        source,
        reason: format!("{error:#}"),
    };
    match record {
        SurfaceRecord::Closed(Ok(value)) => Surface::Closed {
            consumed: value.get("consumed").and_then(Value::as_bool) == Some(true),
        },
        SurfaceRecord::Closed(Err(error)) => unreadable(SurfaceSource::ClosedTombstone, error),
        SurfaceRecord::Closing(Ok(handle)) => Surface::Closing {
            bound: handle.verify_managed_session(session).is_ok(),
        },
        SurfaceRecord::Closing(Err(error)) => unreadable(SurfaceSource::ClosingHandle, error),
        SurfaceRecord::Active(Ok(Some(handle))) => Surface::Active {
            kind: handle.kind,
            supported: handle.kind.supported_on_this_platform(),
            bound: handle.verify_managed_session(session).is_ok(),
            empty_id: handle.id.is_empty(),
        },
        SurfaceRecord::Active(Ok(None)) => Surface::Missing,
        SurfaceRecord::Active(Err(error)) => unreadable(SurfaceSource::ActiveHandle, error),
    }
}

impl Surface {
    /// A tombstone that does not say its handle was consumed is an unreadable part.
    pub(in crate::native) fn unconsumed_tombstone(&self) -> bool {
        matches!(self, Self::Closed { consumed: false })
    }
}

/// Whether the recorded active surface is there, judged on request against the OS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum SurfacePresence {
    Present,
    Missing,
    Unverified(String),
}

impl SurfacePresence {
    pub(in crate::native) fn of(probe: Result<bool>) -> Self {
        match probe {
            Ok(true) => Self::Present,
            Ok(false) => Self::Missing,
            Err(error) => Self::Unverified(format!("{error:#}")),
        }
    }
}

/// An auxiliary Record the Observation could not read; the lifecycle records fail the
/// whole read instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum UnreadablePart {
    Hold,
    Owner,
    SurfaceRecord,
    UnconsumedCloseTombstone,
    ResumedFrom,
    RequestIndex,
}

pub(in crate::native) fn unreadable_parts(
    held: &Result<bool>,
    owner: &Result<Option<NativeSessionOwner>>,
    surface: &Surface,
    resumed_from: &Result<Option<ResumedFrom>>,
    request_index: Option<&IndexIncomplete>,
) -> Vec<UnreadablePart> {
    let mut parts = Vec::new();
    if held.is_err() {
        parts.push(UnreadablePart::Hold);
    }
    if owner.is_err() {
        parts.push(UnreadablePart::Owner);
    }
    if matches!(surface, Surface::Unreadable { .. }) {
        parts.push(UnreadablePart::SurfaceRecord);
    }
    if surface.unconsumed_tombstone() {
        parts.push(UnreadablePart::UnconsumedCloseTombstone);
    }
    if resumed_from.is_err() {
        parts.push(UnreadablePart::ResumedFrom);
    }
    if request_index.is_some() {
        parts.push(UnreadablePart::RequestIndex);
    }
    parts
}

/// What the next `result` command should address: the active Request, the list while a
/// Claim has no Receipt, the latest Result the caller already decoded, or nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum ResultCandidate {
    ActiveRequest(String),
    ClaimOnly,
    LatestDecoded(String),
    None,
}

/// `latest_decoded` is the event id of a latest Result the caller has already decoded;
/// this function decodes nothing.
pub(in crate::native) fn result_candidate(
    records: &Snapshot,
    latest_decoded: Option<&str>,
) -> ResultCandidate {
    if let Some(receipt) = records.active_receipt() {
        ResultCandidate::ActiveRequest(receipt.request_id.clone())
    } else if records.claim.is_some() {
        ResultCandidate::ClaimOnly
    } else {
        latest_decoded.map_or(ResultCandidate::None, |event| {
            ResultCandidate::LatestDecoded(event.to_owned())
        })
    }
}

/// The judgments an Observation makes eagerly from its Records. The owner and the surface
/// presence are judged on request (`SessionEvidence`).
pub(in crate::native) struct Judgments {
    pub(in crate::native) turn: Turn,
    /// The active Request's state, observed independently of the turn.
    pub(in crate::native) active_state: Option<Result<RequestState>>,
    pub(in crate::native) claim_without_receipt: bool,
    pub(in crate::native) recovery_required: bool,
    pub(in crate::native) request_index: Option<IndexIncomplete>,
    pub(in crate::native) residual_surface: Option<ResidualSurface>,
    pub(in crate::native) lifecycle: Lifecycle,
    pub(in crate::native) hold: Hold,
    pub(in crate::native) cancel: Cancel,
    pub(in crate::native) surface: Surface,
    pub(in crate::native) unreadable: Vec<UnreadablePart>,
}

impl Judgments {
    pub(in crate::native) fn of(
        records: &Snapshot,
        evidence: &SessionEvidence,
        cancel: Option<&CancelObservation>,
        resumed_from: &Result<Option<ResumedFrom>>,
        launch_failure: Option<(&'static str, String)>,
        active_state: Option<Result<RequestState>>,
        session: &str,
    ) -> Self {
        let request_index = self::request_index(records);
        let surface = self::surface(&evidence.surface, session);
        let unreadable = unreadable_parts(
            &evidence.held,
            &evidence.owner,
            &surface,
            resumed_from,
            request_index.as_ref(),
        );
        Self {
            turn: turn(records, launch_failure),
            active_state,
            claim_without_receipt: claim_without_receipt(records),
            recovery_required: recovery_required(records),
            request_index,
            residual_surface: residual_surface(records),
            lifecycle: lifecycle(records),
            hold: hold(
                &evidence.held,
                evidence.close_tombstone.as_ref(),
                Some(&records.status.state),
            ),
            cancel: self::cancel(cancel),
            surface,
            unreadable,
        }
    }
}

#[cfg(test)]
pub(in crate::native) mod tests {
    use super::*;
    use crate::native::session::launch::{self, Phase};
    use crate::native::session::requests::Receipt;
    use crate::native::session::schema::{SessionEvent, SessionManifest};
    use crate::native::session::turn::PendingTurnCompletion;
    use crate::native::terminal::TerminalSession;
    use ControlAttestation::*;
    use serde_json::json;
    use std::cell::OnceCell;

    pub(in crate::native) const SESSION: &str = "session-test";
    pub(in crate::native) const CLAIM: &str = "1-2-3";

    pub(in crate::native) fn attestation(
        process: ProcessObservation,
        control: ControlAttestation,
        pid_alive: Option<bool>,
    ) -> OwnerAttestation {
        OwnerAttestation {
            process,
            control,
            pid_alive,
        }
    }

    fn live(control: ControlAttestation) -> OwnerAttestation {
        attestation(
            ProcessObservation::Live(Attested::Incarnation),
            control,
            Some(true),
        )
    }

    fn projection(judgment: &OwnerJudgment) -> Value {
        let o = judgment.observation();
        json!([o.process_alive, o.identity_matches, o.error])
    }

    fn record(binding: Option<&str>) -> Result<Option<NativeSessionOwner>> {
        Ok(Some(NativeSessionOwner {
            pid: 4242,
            managed_session_id: binding.map(str::to_owned),
            ..Default::default()
        }))
    }

    /// An in-memory Snapshot of a session in `state` with no Claim, Receipt or journal.
    pub(in crate::native) fn snapshot(state: SessionState) -> Snapshot {
        Snapshot {
            manifest: SessionManifest {
                schema: 1,
                id: SESSION.to_owned(),
                provider: "codex".to_owned(),
                provider_path: std::env::temp_dir().join("codex"),
                provider_version: "0.0.0".to_owned(),
                workspace: std::env::temp_dir(),
                title: "judgment".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            },
            status: SessionStatus {
                state,
                generation: 1,
                updated_unix_ms: 2,
                exit_code: None,
                error: None,
                residual_surface: None,
            },
            receipts: Vec::new(),
            unreadable_requests: 0,
            request_index_error: None,
            paths: Vec::new(),
            claim: None,
            pending: None,
            launch: None,
            pending_event: None,
            _lock: None,
        }
    }

    pub(in crate::native) fn receipt(claim: &str) -> Receipt {
        Receipt {
            schema: 1,
            request_id: "request-test".to_owned(),
            claim_token: claim.to_owned(),
            event_file: "event-test.json".to_owned(),
            created_unix_ms: Some(1),
            source: None,
            context_sources: Vec::new(),
        }
    }

    fn pending(claim: &str) -> PendingTurnCompletion {
        PendingTurnCompletion::new(
            claim,
            SessionEvent {
                provenance: None,
                cancelled: false,
                provider: "codex".to_owned(),
                message: "done".to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: Some(3),
                usage: None,
                usage_error: None,
            },
            None,
        )
        .unwrap()
    }

    /// Evidence with every auxiliary record readable and absent.
    pub(in crate::native) fn evidence() -> SessionEvidence {
        SessionEvidence {
            owner: Ok(None),
            held: Ok(false),
            close_tombstone: None,
            surface: SurfaceRecord::Active(Ok(None)),
            owner_judgment: OnceCell::new(),
            surface_presence: OnceCell::new(),
        }
    }

    pub(in crate::native) fn handle(kind: TerminalKind, binding: Option<&str>) -> TerminalSession {
        TerminalSession {
            kind,
            id: "surface-1".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: binding.map(str::to_owned),
            windows_process_identity: None,
            wezterm_mux: None,
        }
    }

    fn error(text: &str) -> anyhow::Error {
        anyhow::anyhow!("{text}")
    }

    #[test]
    fn bound_rows_keep_the_attestation_and_decide_death_from_it_alone() {
        type Row = (fn() -> OwnerAttestation, BoundOwner, Value, bool);
        let rows: [Row; 9] = [
            (
                || live(Verified),
                BoundOwner::Verified,
                json!([true, true, null]),
                false,
            ),
            (
                || live(Changed("owner TTY changed".into())),
                BoundOwner::ControlChanged,
                json!([true, false, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Live(Attested::Incarnation),
                        Unknown("no TTY".into()),
                        Some(false),
                    )
                },
                BoundOwner::Unknown,
                json!([false, null, "no TTY"]),
                false,
            ),
            (
                || live(NotRecorded),
                BoundOwner::Uncompared,
                json!([true, null, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Live(Attested::PidOnly),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Uncompared,
                json!([true, null, null]),
                false,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Gone(Gone::Exited),
                        NotRecorded,
                        Some(false),
                    )
                },
                BoundOwner::Exited,
                json!([false, null, null]),
                true,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Gone(Gone::Reused("other birth".into())),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Reused,
                json!([true, false, null]),
                true,
            ),
            (
                || {
                    attestation(
                        ProcessObservation::Unknown("denied".into()),
                        NotRecorded,
                        Some(true),
                    )
                },
                BoundOwner::Unknown,
                json!([true, null, "denied"]),
                false,
            ),
            // A false PID hint beside an Unknown attestation is not a death (D15).
            (
                || {
                    attestation(
                        ProcessObservation::Unknown("denied".into()),
                        NotRecorded,
                        Some(false),
                    )
                },
                BoundOwner::Unknown,
                json!([false, null, "denied"]),
                false,
            ),
        ];
        for (index, (make, verdict, expected, gone)) in rows.into_iter().enumerate() {
            let judgment = owner(
                &record(Some(SESSION)),
                SESSION,
                &Observers {
                    attest: &|_| make(),
                    pid_alive: &|_| panic!("a bound record is attested, not PID-observed"),
                },
            );
            assert!(matches!(judgment, OwnerJudgment::Bound(_)), "{index}");
            assert_eq!(judgment.bound(), Some(verdict), "{index}");
            assert_eq!(projection(&judgment), expected, "{index}");
            assert_eq!(judgment.gone(), gone, "{index}");
        }
    }

    #[test]
    fn unbound_foreign_absent_and_unreadable_records_are_never_attested() {
        let no_attestation = |_: &NativeSessionOwner| -> OwnerAttestation {
            panic!("only a bound record is attested")
        };
        for (alive, expected, gone) in [
            (Some(true), json!([true, null, null]), false),
            (Some(false), json!([false, null, null]), true),
            (None, json!([null, null, null]), false),
        ] {
            let judgment = owner(
                &record(None),
                SESSION,
                &Observers {
                    attest: &no_attestation,
                    pid_alive: &|pid| {
                        assert_eq!(pid, 4242);
                        alive
                    },
                },
            );
            assert!(matches!(judgment, OwnerJudgment::UnboundLegacy { alive: a } if a == alive));
            assert_eq!(judgment.bound(), None);
            assert_eq!(projection(&judgment), expected);
            assert_eq!(judgment.gone(), gone);
        }
        let unobserved = Observers {
            attest: &no_attestation,
            pid_alive: &|_| panic!("a foreign, absent or unreadable record is not observed"),
        };
        let unreadable = Err(anyhow::anyhow!("invalid JSON").context("owner record"));
        for (read, expected) in [
            (
                record(Some("session-other")),
                json!([null, null, FOREIGN_OWNER]),
            ),
            (Ok(None), json!([null, null, null])),
            (
                unreadable,
                json!([null, null, "owner record: invalid JSON"]),
            ),
        ] {
            let judgment = owner(&read, SESSION, &unobserved);
            match &judgment {
                OwnerJudgment::Foreign => assert!(read.as_ref().is_ok_and(Option::is_some)),
                OwnerJudgment::Absent => assert!(read.as_ref().is_ok_and(Option::is_none)),
                OwnerJudgment::Unreadable(reason) => {
                    assert_eq!(reason, "owner record: invalid JSON")
                }
                other => panic!("unexpected judgment {other:?}"),
            }
            assert_eq!(judgment.bound(), None);
            assert_eq!(projection(&judgment), expected);
            assert!(!judgment.gone());
        }
    }

    #[test]
    fn turn_rows_read_claim_error_state_and_journal_and_the_launch_override() {
        let rows: [(&str, SessionState, bool, bool, bool, Turn); 6] = [
            // (name, state, claim, error, pending, expected)
            (
                "no claim",
                SessionState::Ready,
                false,
                false,
                false,
                Turn::NoActiveTurn,
            ),
            (
                "no claim with an error",
                SessionState::Failed,
                false,
                true,
                false,
                Turn::NoActiveTurn,
            ),
            (
                "claim without an error",
                SessionState::Working,
                true,
                false,
                false,
                Turn::TurnInProgress,
            ),
            (
                "claim and error outside working",
                SessionState::Claimed,
                true,
                true,
                false,
                Turn::TurnInProgress,
            ),
            (
                "claim and error with a journal",
                SessionState::Working,
                true,
                true,
                true,
                Turn::TurnInProgress,
            ),
            (
                "claim and error while working",
                SessionState::Working,
                true,
                true,
                false,
                Turn::DeliveryUnconfirmed,
            ),
        ];
        for (name, state, claim, error, journal, expected) in rows {
            let mut records = snapshot(state);
            records.claim = claim.then(|| CLAIM.to_owned());
            records.status.error = error.then(|| "delivery uncertain".to_owned());
            records.pending = journal.then(|| pending(CLAIM));
            assert_eq!(turn(&records, None), expected, "{name}");
            assert_eq!(
                turn(
                    &records,
                    Some(("launch_failed", "provider launch failed".into()))
                ),
                Turn::LaunchFailed {
                    reason: "launch_failed",
                    detail: "provider launch failed".into()
                },
                "{name}: the launch diagnostic overrides"
            );
        }
        // The diagnostic itself is the launch module's; a spawned launch yields none.
        let mut records = snapshot(SessionState::Failed);
        records.launch = Some(launch::Record {
            schema: 1,
            claim_token: CLAIM.to_owned(),
            deadline_unix_ms: 1,
            phase: Phase::Spawned,
        });
        let diagnostic = launch::diagnostic(
            records.launch.as_ref(),
            &records.status,
            records.claim.as_deref(),
        );
        assert_eq!(turn(&records, diagnostic), Turn::NoActiveTurn);
    }

    #[test]
    fn claim_receipt_journal_index_residual_and_lifecycle_rows() {
        let mut records = snapshot(SessionState::Working);
        assert!(!claim_without_receipt(&records));
        records.claim = Some(CLAIM.to_owned());
        assert!(claim_without_receipt(&records));
        records.receipts.push(receipt("9-9"));
        assert!(claim_without_receipt(&records));
        records.receipts.push(receipt(CLAIM));
        assert!(!claim_without_receipt(&records));

        assert!(!recovery_required(&records));
        records.pending = Some(pending(CLAIM));
        assert!(recovery_required(&records));

        assert_eq!(request_index(&records), None);
        records.unreadable_requests = 2;
        assert_eq!(
            request_index(&records),
            Some(IndexIncomplete {
                unreadable: 2,
                error: None
            })
        );
        records.unreadable_requests = 0;
        records.request_index_error = Some("requests: not a directory".to_owned());
        assert_eq!(
            request_index(&records),
            Some(IndexIncomplete {
                unreadable: 0,
                error: Some("requests: not a directory".to_owned())
            })
        );

        for (recorded, error, expected) in [
            (None, None, None),
            (
                Some(ResidualSurface::Unverified),
                None,
                Some(ResidualSurface::Unverified),
            ),
            (Some(ResidualSurface::Cleared), None, None),
            (
                None,
                Some(format!(
                    "launch failed; {}",
                    launch::RESIDUAL_SURFACE_MARKER
                )),
                Some(ResidualSurface::Unverified),
            ),
            (
                Some(ResidualSurface::Cleared),
                Some(format!(
                    "launch failed; {}",
                    launch::RESIDUAL_SURFACE_MARKER
                )),
                None,
            ),
        ] {
            records.status.residual_surface = recorded;
            records.status.error = error;
            assert_eq!(residual_surface(&records), expected, "{recorded:?}");
        }

        for (state, expected) in [
            (SessionState::Ready, Lifecycle::Ready),
            (SessionState::Failed, Lifecycle::Failed),
            (SessionState::Exited, Lifecycle::Exited),
            (SessionState::Launching, Lifecycle::Other),
            (SessionState::Running, Lifecycle::Other),
            (SessionState::AwaitingInitialInput, Lifecycle::Other),
            (SessionState::Claimed, Lifecycle::Other),
            (SessionState::Working, Lifecycle::Other),
            (SessionState::ResumePending, Lifecycle::Other),
            (SessionState::Closed, Lifecycle::Closed),
            (SessionState::Unknown("odd".into()), Lifecycle::Other),
        ] {
            records.status.state = state.clone();
            assert_eq!(lifecycle(&records), expected, "{state:?}");
        }
    }

    #[test]
    fn hold_rows_carry_the_whole_release_gate() {
        let closed = || SessionStatus {
            state: SessionState::Closed,
            generation: 9,
            updated_unix_ms: 9,
            exit_code: None,
            error: None,
            residual_surface: None,
        };
        let held = |release_allowed| Hold::Held { release_allowed };
        type Row = (
            &'static str,
            Result<bool>,
            Option<Result<Option<SessionStatus>>>,
            Option<SessionState>,
            Hold,
        );
        let rows: Vec<Row> = vec![
            (
                "held, no tombstone, working",
                Ok(true),
                Some(Ok(None)),
                Some(SessionState::Working),
                held(true),
            ),
            (
                "held, tombstone present",
                Ok(true),
                Some(Ok(Some(closed()))),
                Some(SessionState::Working),
                held(false),
            ),
            (
                "held, tombstone unreadable",
                Ok(true),
                Some(Err(error("closed.json: invalid"))),
                Some(SessionState::Working),
                held(false),
            ),
            (
                "held, tombstone not read",
                Ok(true),
                None,
                Some(SessionState::Working),
                held(false),
            ),
            (
                "held, stored state closed",
                Ok(true),
                Some(Ok(None)),
                Some(SessionState::Closed),
                held(false),
            ),
            (
                "held, stored state unknown (no Snapshot)",
                Ok(true),
                Some(Ok(None)),
                None,
                held(false),
            ),
            (
                "not held",
                Ok(false),
                Some(Ok(None)),
                Some(SessionState::Working),
                Hold::NotHeld,
            ),
            (
                "unreadable",
                Err(error("hold.json: invalid")),
                Some(Ok(None)),
                Some(SessionState::Working),
                Hold::Unreadable("hold.json: invalid".into()),
            ),
        ];
        for (name, held, tombstone, state, expected) in rows {
            assert_eq!(
                hold(&held, tombstone.as_ref(), state.as_ref()),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn cancel_rows_follow_the_record_observation_and_the_fallback_read() {
        let observed = |state: &'static str, judgment: Cancel| CancelObservation {
            request_id: Some("request-test".to_owned()),
            requested_unix_ms: Some(5),
            state,
            error: None,
            judgment,
        };
        assert_eq!(cancel(None), Cancel::Absent);
        for (state, active, expected) in [
            (
                "requested",
                true,
                Cancel::Recorded(Some(CancelState::RequestedActive)),
            ),
            (
                "requested",
                false,
                Cancel::Recorded(Some(CancelState::RequestedStale)),
            ),
            (
                "applied",
                false,
                Cancel::Recorded(Some(CancelState::Applied)),
            ),
            (
                "not_applied",
                false,
                Cancel::Recorded(Some(CancelState::NotApplied)),
            ),
            (
                "unreadable",
                false,
                Cancel::Unreadable("cancel record claim does not match".into()),
            ),
        ] {
            let observation = observed(state, expected.clone());
            assert_eq!(
                cancel(Some(&observation)),
                expected,
                "{state} active={active}"
            );
        }
        assert_eq!(cancel_record(&Ok(None)), Cancel::Absent);
        assert_eq!(
            cancel_record(&Ok(Some(session::cancel::Cancel {
                schema: 1,
                request_id: "request-test".to_owned(),
                claim_token: CLAIM.to_owned(),
                created_unix_ms: 5,
            }))),
            Cancel::Recorded(None)
        );
        assert_eq!(
            cancel_record(&Err(error("cancel.json: invalid"))),
            Cancel::Unreadable("cancel.json: invalid".into())
        );
    }

    #[test]
    fn surface_rows_carry_source_binding_support_and_error_text() {
        #[cfg(target_os = "macos")]
        let (supported, unsupported) = (Some(TerminalKind::Warp), TerminalKind::WindowsConsole);
        #[cfg(windows)]
        let (supported, unsupported) = (Some(TerminalKind::WindowsConsole), TerminalKind::Warp);
        #[cfg(not(any(target_os = "macos", windows)))]
        let (supported, unsupported) = (None::<TerminalKind>, TerminalKind::Warp);
        let unreadable = |source, text: &str| Surface::Unreadable {
            source,
            reason: text.to_owned(),
        };
        let mut rows: Vec<(&str, SurfaceRecord, Surface, Option<ActiveHandle>)> = vec![
            (
                "consumed tombstone",
                SurfaceRecord::Closed(Ok(json!({"consumed": true, "terminal": "warp"}))),
                Surface::Closed { consumed: true },
                None,
            ),
            (
                "tombstone without a consumed fact",
                SurfaceRecord::Closed(Ok(json!({"terminal": "warp"}))),
                Surface::Closed { consumed: false },
                None,
            ),
            (
                "tombstone not consumed",
                SurfaceRecord::Closed(Ok(json!({"consumed": false}))),
                Surface::Closed { consumed: false },
                None,
            ),
            (
                "unreadable tombstone",
                SurfaceRecord::Closed(Err(error("terminal.closed.json: invalid"))),
                unreadable(
                    SurfaceSource::ClosedTombstone,
                    "terminal.closed.json: invalid",
                ),
                None,
            ),
            (
                "bound close handle",
                SurfaceRecord::Closing(Ok(handle(unsupported, Some(SESSION)))),
                Surface::Closing { bound: true },
                None,
            ),
            (
                "close handle of another session",
                SurfaceRecord::Closing(Ok(handle(unsupported, Some("session-other")))),
                Surface::Closing { bound: false },
                None,
            ),
            (
                "unreadable close handle",
                SurfaceRecord::Closing(Err(error("terminal.closing.json: invalid"))),
                unreadable(
                    SurfaceSource::ClosingHandle,
                    "terminal.closing.json: invalid",
                ),
                None,
            ),
            (
                "unsupported active handle, even unbound",
                SurfaceRecord::Active(Ok(Some(handle(unsupported, None)))),
                Surface::Active {
                    kind: unsupported,
                    supported: false,
                    bound: false,
                    empty_id: false,
                },
                Some(ActiveHandle::Unsupported),
            ),
            (
                "no active handle",
                SurfaceRecord::Active(Ok(None)),
                Surface::Missing,
                None,
            ),
            (
                "unreadable active handle",
                SurfaceRecord::Active(Err(error("terminal.json: invalid"))),
                unreadable(SurfaceSource::ActiveHandle, "terminal.json: invalid"),
                None,
            ),
        ];
        if let Some(kind) = supported {
            let mut empty = handle(kind, Some(SESSION));
            empty.id.clear();
            rows.extend([
                (
                    "bound supported handle",
                    SurfaceRecord::Active(Ok(Some(handle(kind, Some(SESSION))))),
                    Surface::Active {
                        kind,
                        supported: true,
                        bound: true,
                        empty_id: false,
                    },
                    Some(ActiveHandle::Bound),
                ),
                (
                    "supported handle of another session",
                    SurfaceRecord::Active(Ok(Some(handle(kind, Some("session-other"))))),
                    Surface::Active {
                        kind,
                        supported: true,
                        bound: false,
                        empty_id: false,
                    },
                    Some(ActiveHandle::BindingUnverified),
                ),
                (
                    "supported handle without a binding",
                    SurfaceRecord::Active(Ok(Some(handle(kind, None)))),
                    Surface::Active {
                        kind,
                        supported: true,
                        bound: false,
                        empty_id: false,
                    },
                    Some(ActiveHandle::BindingUnverified),
                ),
                (
                    "bound supported handle with an empty id",
                    SurfaceRecord::Active(Ok(Some(empty))),
                    Surface::Active {
                        kind,
                        supported: true,
                        bound: true,
                        empty_id: true,
                    },
                    Some(ActiveHandle::BindingUnverified),
                ),
            ]);
        }
        #[cfg(target_os = "macos")]
        rows.push((
            "legacy iTerm2 handle without a binding counts as bound",
            SurfaceRecord::Active(Ok(Some(handle(TerminalKind::Iterm2, None)))),
            Surface::Active {
                kind: TerminalKind::Iterm2,
                supported: true,
                bound: true,
                empty_id: false,
            },
            Some(ActiveHandle::Bound),
        ));
        for (name, record, expected, verdict) in rows {
            let judged = surface(&record, SESSION);
            assert_eq!(judged, expected, "{name}");
            let active = match &judged {
                Surface::Active {
                    supported,
                    bound,
                    empty_id,
                    ..
                } => Some(ActiveHandle::of(*supported, *bound, *empty_id)),
                _ => None,
            };
            assert_eq!(active, verdict, "{name}");
            assert_eq!(
                judged.unconsumed_tombstone(),
                matches!(expected, Surface::Closed { consumed: false }),
                "{name}"
            );
        }
        assert_eq!(SurfacePresence::of(Ok(true)), SurfacePresence::Present);
        assert_eq!(SurfacePresence::of(Ok(false)), SurfacePresence::Missing);
        assert_eq!(
            SurfacePresence::of(Err(error("probe deadline exhausted"))),
            SurfacePresence::Unverified("probe deadline exhausted".into())
        );
    }

    #[test]
    fn unreadable_parts_name_each_part_once() {
        let readable = evidence();
        let surface = Surface::Missing;
        assert!(
            unreadable_parts(&readable.held, &readable.owner, &surface, &Ok(None), None).is_empty()
        );
        let unreadable_owner: Result<Option<NativeSessionOwner>> = Err(error("owner"));
        let resumed: Result<Option<ResumedFrom>> = Err(error("resumed_from"));
        let index = IndexIncomplete {
            unreadable: 1,
            error: None,
        };
        for (name, held, owner, surface, resumed_from, index, expected) in [
            (
                "hold",
                &Err(error("hold")),
                &readable.owner,
                &surface,
                &Ok(None),
                None,
                vec![UnreadablePart::Hold],
            ),
            (
                "owner",
                &readable.held,
                &unreadable_owner,
                &surface,
                &Ok(None),
                None,
                vec![UnreadablePart::Owner],
            ),
            (
                "surface record",
                &readable.held,
                &readable.owner,
                &Surface::Unreadable {
                    source: SurfaceSource::ActiveHandle,
                    reason: "x".into(),
                },
                &Ok(None),
                None,
                vec![UnreadablePart::SurfaceRecord],
            ),
            (
                "unconsumed tombstone",
                &readable.held,
                &readable.owner,
                &Surface::Closed { consumed: false },
                &Ok(None),
                None,
                vec![UnreadablePart::UnconsumedCloseTombstone],
            ),
            (
                "consumed tombstone is readable",
                &readable.held,
                &readable.owner,
                &Surface::Closed { consumed: true },
                &Ok(None),
                None,
                vec![],
            ),
            (
                "resumed_from",
                &readable.held,
                &readable.owner,
                &surface,
                &resumed,
                None,
                vec![UnreadablePart::ResumedFrom],
            ),
            (
                "request index",
                &readable.held,
                &readable.owner,
                &surface,
                &Ok(None),
                Some(&index),
                vec![UnreadablePart::RequestIndex],
            ),
            (
                "every part",
                &Err(error("hold")),
                &unreadable_owner,
                &Surface::Unreadable {
                    source: SurfaceSource::ClosedTombstone,
                    reason: "x".into(),
                },
                &resumed,
                Some(&index),
                vec![
                    UnreadablePart::Hold,
                    UnreadablePart::Owner,
                    UnreadablePart::SurfaceRecord,
                    UnreadablePart::ResumedFrom,
                    UnreadablePart::RequestIndex,
                ],
            ),
        ] {
            assert_eq!(
                unreadable_parts(held, owner, surface, resumed_from, index),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn result_candidates_address_the_request_then_the_list_then_the_decoded_latest() {
        let mut records = snapshot(SessionState::Working);
        records.claim = Some(CLAIM.to_owned());
        records.receipts.push(receipt(CLAIM));
        for latest in [None, Some("event-9.json")] {
            assert_eq!(
                result_candidate(&records, latest),
                ResultCandidate::ActiveRequest("request-test".into())
            );
        }
        records.receipts.clear();
        for latest in [None, Some("event-9.json")] {
            assert_eq!(
                result_candidate(&records, latest),
                ResultCandidate::ClaimOnly
            );
        }
        records.claim = None;
        assert_eq!(
            result_candidate(&records, Some("event-9.json")),
            ResultCandidate::LatestDecoded("event-9.json".into())
        );
        assert_eq!(result_candidate(&records, None), ResultCandidate::None);
    }

    #[test]
    fn judgments_assemble_from_the_records_and_the_evidence() {
        let mut records = snapshot(SessionState::Working);
        records.claim = Some(CLAIM.to_owned());
        records.status.error = Some("delivery uncertain".to_owned());
        records.unreadable_requests = 1;
        let mut evidence = evidence();
        evidence.held = Ok(true);
        evidence.close_tombstone = Some(Ok(None));
        evidence.surface = SurfaceRecord::Closed(Ok(json!({"consumed": false})));
        let judgments = Judgments::of(
            &records,
            &evidence,
            None,
            &Err(error("manifest: resumed_from")),
            None,
            Some(Ok(RequestState::Pending)),
            SESSION,
        );
        assert_eq!(judgments.turn, Turn::DeliveryUnconfirmed);
        assert!(matches!(
            judgments.active_state,
            Some(Ok(RequestState::Pending))
        ));
        assert!(judgments.claim_without_receipt);
        assert!(!judgments.recovery_required);
        assert_eq!(
            judgments.request_index,
            Some(IndexIncomplete {
                unreadable: 1,
                error: None
            })
        );
        assert_eq!(judgments.residual_surface, None);
        assert_eq!(judgments.lifecycle, Lifecycle::Other);
        assert_eq!(
            judgments.hold,
            Hold::Held {
                release_allowed: true
            }
        );
        assert_eq!(judgments.cancel, Cancel::Absent);
        assert_eq!(judgments.surface, Surface::Closed { consumed: false });
        assert_eq!(
            judgments.unreadable,
            vec![
                UnreadablePart::UnconsumedCloseTombstone,
                UnreadablePart::ResumedFrom,
                UnreadablePart::RequestIndex,
            ]
        );
        // The launch override and the stored state reach the turn and hold judgments.
        records.status.state = SessionState::Closed;
        let judgments = Judgments::of(
            &records,
            &evidence,
            None,
            &Ok(None),
            Some(("launch_failed", "provider launch failed".into())),
            None,
            SESSION,
        );
        assert!(matches!(
            judgments.turn,
            Turn::LaunchFailed {
                reason: "launch_failed",
                ..
            }
        ));
        assert_eq!(
            judgments.hold,
            Hold::Held {
                release_allowed: false
            }
        );
        assert_eq!(
            judgments.unreadable,
            vec![
                UnreadablePart::UnconsumedCloseTombstone,
                UnreadablePart::RequestIndex
            ]
        );
    }
}
