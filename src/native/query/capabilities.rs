//! What a session's own action admissions would decide at this Snapshot. An observation is
//! not a reservation: the action rechecks every gate under its own lock, and an admissible
//! cancel records intent only, never interruption or completion. `tell` is observed through
//! its Bridge record gates only; the gates it does not evaluate are named, never inferred.
use super::*;
use crate::native::session::cancel::{self, Refusal, RefusalReason};
use crate::native::session::{self, close, hold};
use observation::judgment::{ActiveHandle, BoundOwner, OwnerJudgment, Surface};

pub(super) const SCHEMA_VERSION: u32 = 1;

/// A legacy session without a lifecycle lock has no Snapshot that excludes writers.
const LOCK_ABSENT: &str = "lifecycle_lock_absent";

/// The Observation's own evidence and judgments the follow-up gates map: read under the
/// Snapshot's lock and judged once for the whole inspection.
pub(super) struct FollowUpEvidence<'a> {
    pub(super) held: &'a Result<bool>,
    pub(super) recovery_required: bool,
    pub(super) owner: &'a OwnerJudgment,
    pub(super) surface: &'a Surface,
    pub(super) reopened: bool,
}

/// Called only while the caller retains the Snapshot's lifecycle lock; it takes no lock,
/// writes nothing and starts no provider process. `observed_unix_ms` is the time of this
/// observation, not of any record.
pub(super) fn value(
    reader: &Reader,
    records: &Snapshot,
    id: &str,
    observed_unix_ms: u128,
    follow_up: &FollowUpEvidence<'_>,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "observed_unix_ms": observed_unix_ms,
        "cancel": cancel_value(reader, records, id),
        "tell": tell_value(reader, records, id, follow_up),
        "questions": questions_value(reader, records),
    })
}

fn cancel_value(reader: &Reader, records: &Snapshot, id: &str) -> Value {
    if records._lock.is_none() {
        return json!({"state": "unknown", "reason": LOCK_ABSENT});
    }
    let support = || {
        provider::cancel_support(
            FirstPartyCli::from_str(&records.manifest.provider).map_err(anyhow::Error::msg)?,
            reader.directory(),
        )
    };
    match cancel::admission(reader, id, support) {
        Ok(admitted) => json!({
            "state": "admissible", "reason": "admission_passed",
            "request_id": admitted.request_id,
        }),
        Err(error) => match error.downcast_ref::<Refusal>() {
            Some(refusal) => {
                let mut value = json!({
                    "state": if refusal.reason() == RefusalReason::ProviderUnsupported {
                        "unsupported"
                    } else {
                        "refused"
                    },
                    "reason": refusal.reason().code(),
                    "detail": refusal.to_string(),
                });
                if let Some(request) = refusal.request_id() {
                    value["request_id"] = json!(request);
                }
                value
            }
            None => json!({
                "state": "unknown", "reason": "evidence_unreadable",
                "detail": format!("{error:#}"),
            }),
        },
    }
}

/// The first follow-up gate that did not pass.
enum Stop {
    Refused {
        reason: &'static str,
        detail: String,
    },
    Unknown {
        reason: &'static str,
        detail: Option<String>,
    },
}

impl Stop {
    fn refused(reason: &'static str, detail: impl Into<String>) -> Self {
        Self::Refused {
            reason,
            detail: detail.into(),
        }
    }

    fn unknown(reason: &'static str, detail: Option<String>) -> Self {
        Self::Unknown { reason, detail }
    }

    fn unreadable(detail: String) -> Self {
        Self::unknown("evidence_unreadable", Some(detail))
    }

    fn value(self) -> Value {
        match self {
            Self::Refused { reason, detail } => {
                json!({"state": "refused", "reason": reason, "detail": detail})
            }
            Self::Unknown { reason, detail } => {
                let mut value = json!({"state": "unknown", "reason": reason});
                if let Some(detail) = detail {
                    value["detail"] = json!(detail);
                }
                value
            }
        }
    }
}

/// The action's own refusal: the same code path decides it and renders its text.
impl From<hold::FollowUpRefusal> for Stop {
    fn from(refusal: hold::FollowUpRefusal) -> Self {
        Self::refused(refusal.code(), refusal.to_string())
    }
}

fn tell_value(
    reader: &Reader,
    records: &Snapshot,
    id: &str,
    evidence: &FollowUpEvidence<'_>,
) -> Value {
    if records._lock.is_none() {
        return json!({"state": "unknown", "reason": LOCK_ABSENT});
    }
    match tell_gates(reader, records, id, evidence) {
        Ok(unevaluated) => json!({
            "state": "admissible", "reason": "record_gates_passed",
            "unevaluated": unevaluated,
        }),
        Err(stop) => stop.value(),
    }
}

/// `tell`'s Bridge record gates in the action's order, stopping at the first that does not
/// pass. On success, returns the gates this observation does not evaluate, in the action's
/// order: the terminal surface probe, reopened-conversation exclusivity, and the provider's
/// delivery readiness with the adapter gates it leaves to delivery.
fn tell_gates(
    reader: &Reader,
    records: &Snapshot,
    id: &str,
    evidence: &FollowUpEvidence<'_>,
) -> std::result::Result<Vec<&'static str>, Stop> {
    // `tell` checks the Hold before it changes any record.
    hold::hold_gate(evidence.held, id)?;
    // What convergence would find, without performing its recovery or repair. Recovery can
    // publish the pending turn and change every later gate.
    if evidence.recovery_required {
        return Err(Stop::unknown("recovery_required", None));
    }
    if close::repair_examines_owner(&records.status.state) {
        owner_gate(evidence.owner)?;
    }
    let cli = FirstPartyCli::from_str(&records.manifest.provider).map_err(Stop::unreadable)?;
    surface_gate(evidence.surface)?;
    owner_control_gate(evidence.owner, cfg!(target_os = "macos"))?;
    hold::ready_gate(&records.status.state, id)?;
    if records.claim.is_some() {
        return Err(Stop::refused("turn_claimed", session::TURN_ALREADY_CLAIMED));
    }
    let adapter = match provider::follow_up_static(cli, reader) {
        provider::FollowUpStatic::Ok { unevaluated } => unevaluated,
        provider::FollowUpStatic::Refused { reason, detail } => {
            return Err(Stop::refused(reason, detail));
        }
        provider::FollowUpStatic::Unknown(detail) => return Err(Stop::unreadable(detail)),
    };
    let mut unevaluated = vec!["terminal_surface_probe"];
    if evidence.reopened {
        unevaluated.push("reopened_conversation_exclusivity");
    }
    unevaluated.push("provider_delivery_readiness");
    unevaluated.extend_from_slice(adapter);
    Ok(unevaluated)
}

/// The liveness verdict dead-owner repair would act on, judged where convergence runs. A
/// bound record is judged by repair's own liveness rule; an unbound record observed gone is
/// the same death the repair finds. Repair reads an unreadable record and fails there. Any
/// other record lets convergence continue, and is judged with the surface ownership below.
fn owner_gate(owner: &OwnerJudgment) -> std::result::Result<(), Stop> {
    match owner {
        OwnerJudgment::Bound(attestation) => match close::repair_owner_is_live(attestation) {
            Ok(true) => Ok(()),
            Ok(false) => Err(owner_ended()),
            Err(error) => Err(Stop::unknown(
                "owner_unverified",
                Some(format!("{error:#}")),
            )),
        },
        owner if owner.gone() => Err(owner_ended()),
        OwnerJudgment::Unreadable(reason) => Err(Stop::unreadable(reason.clone())),
        OwnerJudgment::UnboundLegacy { .. } | OwnerJudgment::Foreign | OwnerJudgment::Absent => {
            Ok(())
        }
    }
}

/// The owner side of `tell`'s surface ownership check, judged after the recorded handle as
/// the probe that verifies it runs after `tell` reads that handle. The probe requires an
/// owner bound to this session, so any other record is unattested. An attestation that
/// could not inspect its control identity is unverified everywhere. On macOS the probe
/// also requires the recorded birth, TTY and process groups to match the live owner, so a
/// changed or uncompared control identity is unknown there. None of these is a death, so
/// none is refused; a death and an unreadable record are the liveness gate's verdicts.
fn owner_control_gate(owner: &OwnerJudgment, macos: bool) -> std::result::Result<(), Stop> {
    match owner {
        OwnerJudgment::Bound(attestation) => match BoundOwner::of(attestation) {
            // The identity could not be read; its projection carries why.
            BoundOwner::Unknown => Err(Stop::unknown(
                "owner_unverified",
                session::OwnerObservation::from(attestation).error,
            )),
            BoundOwner::ControlChanged | BoundOwner::Uncompared if macos => {
                Err(Stop::unknown("owner_control_unverified", None))
            }
            _ => Ok(()),
        },
        owner if owner.gone() => Ok(()),
        OwnerJudgment::Unreadable(_) => Ok(()),
        OwnerJudgment::UnboundLegacy { .. } | OwnerJudgment::Foreign | OwnerJudgment::Absent => {
            Err(Stop::unknown("owner_unattested", None))
        }
    }
}

fn owner_ended() -> Stop {
    Stop::refused(
        "owner_ended",
        "the recorded session owner is no longer running",
    )
}

/// The recorded surface as the action reads it before its probe, judged once by the
/// Observation (`judgment::surface`).
fn surface_gate(surface: &Surface) -> std::result::Result<(), Stop> {
    match surface {
        Surface::Missing => Err(Stop::refused(
            "surface_missing",
            "no active terminal handle is recorded",
        )),
        Surface::Closed { .. } | Surface::Closing { .. } => Err(Stop::refused(
            "surface_closed",
            "the terminal surface is closed or being closed",
        )),
        Surface::Active {
            supported,
            bound,
            empty_id,
            ..
        } => match ActiveHandle::of(*supported, *bound, *empty_id) {
            ActiveHandle::Bound => Ok(()),
            ActiveHandle::Unsupported => Err(Stop::refused(
                "surface_unsupported",
                "the recorded terminal is not supported on this platform",
            )),
            ActiveHandle::BindingUnverified => Err(Stop::refused(
                "surface_unbound",
                "the terminal handle is not bound to this session",
            )),
        },
        Surface::Unreadable { reason, .. } => Err(Stop::unreadable(reason.clone())),
    }
}

/// Whether this session was configured to observe question emissions, and whether Bridge
/// can answer one; each adapter states both.
fn questions_value(reader: &Reader, records: &Snapshot) -> Value {
    let unknown = |reason: &str, detail: Option<&str>| {
        let mut value = json!({"state": "unknown", "reason": reason});
        if let Some(detail) = detail {
            value["detail"] = json!(detail);
        }
        json!({"emission_observation": value, "response": value})
    };
    if records._lock.is_none() {
        return unknown(LOCK_ABSENT, None);
    }
    let cli = match FirstPartyCli::from_str(&records.manifest.provider) {
        Ok(cli) => cli,
        Err(error) => return unknown("evidence_unreadable", Some(error.as_str())),
    };
    let capability = provider::question_capability(cli, reader);
    let emission = match capability.emission {
        provider::EmissionObservation::Configured(reason) => {
            json!({"state": "configured", "reason": reason})
        }
        provider::EmissionObservation::Unavailable(reason) => {
            json!({"state": "unavailable", "reason": reason})
        }
        provider::EmissionObservation::Unsupported(reason) => {
            json!({"state": "unsupported", "reason": reason})
        }
        provider::EmissionObservation::Unknown(detail) => {
            json!({"state": "unknown", "reason": "evidence_unreadable", "detail": detail})
        }
    };
    let response = match capability.response {
        provider::QuestionResponse::Unsupported(reason) => {
            json!({"state": "unsupported", "reason": reason})
        }
    };
    json!({"emission_observation": emission, "response": response})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::process_identity::{Attested, Gone, ProcessObservation};
    use crate::native::query::observation::judgment::tests::attestation;
    use crate::native::session::owner::ControlAttestation;

    /// The liveness gate, then (past the recorded handle) the ownership gate, as tell_gates
    /// orders them.
    fn gate(process: ProcessObservation, control: ControlAttestation, macos: bool) -> Value {
        let owner = OwnerJudgment::Bound(attestation(process, control, Some(true)));
        match owner_gate(&owner).and_then(|()| owner_control_gate(&owner, macos)) {
            Ok(()) => json!("pass"),
            Err(stop) => stop.value(),
        }
    }

    // A record that lets convergence continue passes the liveness gate and is judged by
    // the ownership gate; a death or an unreadable record is the liveness gate's verdict.
    #[test]
    fn the_liveness_gate_stops_only_where_repair_would() {
        for owner in [
            OwnerJudgment::UnboundLegacy { alive: Some(true) },
            OwnerJudgment::UnboundLegacy { alive: None },
            OwnerJudgment::Foreign,
            OwnerJudgment::Absent,
        ] {
            assert!(owner_gate(&owner).is_ok(), "{owner:?}");
            for macos in [false, true] {
                assert_eq!(
                    owner_control_gate(&owner, macos).unwrap_err().value(),
                    json!({"state": "unknown", "reason": "owner_unattested"}),
                    "{owner:?}"
                );
            }
        }
        let gone = OwnerJudgment::UnboundLegacy { alive: Some(false) };
        assert_eq!(
            owner_gate(&gone).unwrap_err().value()["reason"],
            "owner_ended"
        );
        let unreadable = OwnerJudgment::Unreadable("invalid JSON".into());
        assert_eq!(
            owner_gate(&unreadable).unwrap_err().value(),
            json!({"state": "unknown", "reason": "evidence_unreadable", "detail": "invalid JSON"})
        );
        for macos in [false, true] {
            assert!(owner_control_gate(&gone, macos).is_ok());
            assert!(owner_control_gate(&unreadable, macos).is_ok());
        }
    }

    // Both platform mappings, on every host: the macOS rows are what tell's own surface
    // probe can verify there, and the other platforms keep repair's liveness rule.
    #[test]
    fn a_live_bound_owner_passes_only_where_the_probe_could_verify_it() {
        let unverified =
            json!({"state": "unknown", "reason": "owner_unverified", "detail": "tty unreadable"});
        let control = json!({"state": "unknown", "reason": "owner_control_unverified"});
        let ended = json!({"state": "refused", "reason": "owner_ended",
            "detail": "the recorded session owner is no longer running"});
        for macos in [false, true] {
            let on_macos = |verdict: &Value| {
                if macos {
                    verdict.clone()
                } else {
                    json!("pass")
                }
            };
            let live = || ProcessObservation::Live(Attested::Incarnation);
            let rows = [
                (live(), ControlAttestation::Verified, json!("pass")),
                (
                    live(),
                    ControlAttestation::Unknown("tty unreadable".into()),
                    unverified.clone(),
                ),
                (
                    live(),
                    ControlAttestation::Changed("process group changed".into()),
                    on_macos(&control),
                ),
                (live(), ControlAttestation::NotRecorded, on_macos(&control)),
                (
                    ProcessObservation::Live(Attested::PidOnly),
                    ControlAttestation::NotRecorded,
                    on_macos(&control),
                ),
                (
                    ProcessObservation::Gone(Gone::Exited),
                    ControlAttestation::NotRecorded,
                    ended.clone(),
                ),
                (
                    ProcessObservation::Gone(Gone::Reused("another birth".into())),
                    ControlAttestation::NotRecorded,
                    ended.clone(),
                ),
                (
                    ProcessObservation::Unknown("access denied".into()),
                    ControlAttestation::NotRecorded,
                    json!({"state": "unknown", "reason": "owner_unverified",
                        "detail": "access denied"}),
                ),
            ];
            for (process, control_attestation, expected) in rows {
                let label = format!("{macos} {process:?} {control_attestation:?}");
                assert_eq!(
                    gate(process, control_attestation, macos),
                    expected,
                    "{label}"
                );
            }
        }
    }
}
