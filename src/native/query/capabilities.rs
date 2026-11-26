//! What a session's own action admissions would decide at this Snapshot. An observation is
//! not a reservation: the action rechecks every gate under its own lock, and an admissible
//! cancel records intent only, never interruption or completion. `tell` is observed through
//! its TAB record gates only; the gates it does not evaluate are named, never inferred.
use super::*;
use crate::native::session::cancel::{self, Refusal, RefusalReason};
use crate::native::session::owner::OwnerAttestation;
use crate::native::session::{self, close, hold};
use crate::native::terminal::ownership::{OwnerRequirement, SurfaceProbe};
use observation::judgment::{ActiveHandle, OwnerJudgment, Surface};

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

/// `tell`'s TAB record gates in the action's order, stopping at the first that does not
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
    // A Session whose own Records say it belongs to another OS (`terminal::foreign`).
    if let Some(elsewhere) = terminal::foreign::of_records(reader) {
        return Err(tell_gate_elsewhere(
            reader, records, id, evidence, elsewhere,
        ));
    }
    if close::repair_examines_owner(&records.status.state) {
        owner_gate(evidence.owner)?;
    }
    let cli = FirstPartyCli::from_str(&records.manifest.provider).map_err(Stop::unreadable)?;
    surface_gate(evidence.surface)?;
    owner_control_gate(evidence.owner, |attestation| {
        SurfaceProbe::HOST.owner_requirement(attestation)
    })?;
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

/// `tell`'s gate for a Session whose own Records say it belongs to another OS: Repair judges
/// no owner for it, so the owner gate passes; an active handle is refused as `tell`'s
/// ownership proof refuses it, before any terminal call and in the one order (a closed
/// Session, then a handle that is not this Session's, then another OS); without one, the
/// recorded surface's own refusal.
fn tell_gate_elsewhere(
    reader: &Reader,
    records: &Snapshot,
    id: &str,
    evidence: &FollowUpEvidence<'_>,
    elsewhere: terminal::foreign::Elsewhere,
) -> Stop {
    if let Err(error) = FirstPartyCli::from_str(&records.manifest.provider) {
        return Stop::unreadable(error);
    }
    let Surface::Active {
        bound, empty_id, ..
    } = evidence.surface
    else {
        return surface_gate(evidence.surface)
            .expect_err("only an active handle passes the surface gate");
    };
    if terminal::foreign::closed(reader) {
        return Stop::refused(
            "state_not_ready",
            format!("session {id} is closed; its terminal is not addressed"),
        );
    }
    if !bound || *empty_id {
        return Stop::refused(
            "surface_unbound",
            "the terminal handle is not bound to this session",
        );
    }
    Stop::refused(
        "surface_unsupported",
        format!("{:#}", elsewhere.refusal(reader.directory())),
    )
}

/// The liveness verdict dead-owner repair would act on, judged where convergence runs.
/// Repair reads the Owner record under the same read policy and attests the recorded process
/// whatever Session the record names, so every recorded owner (bound, unbound or foreign) is
/// judged by Repair's own liveness rule: a death is Repair's, and an attestation that cannot
/// decide fails Repair. An unreadable record fails Repair too. Without a record convergence
/// continues; a live record and an absent one are judged with the surface ownership below.
fn owner_gate(owner: &OwnerJudgment) -> std::result::Result<(), Stop> {
    match owner {
        OwnerJudgment::Bound(attestation)
        | OwnerJudgment::UnboundLegacy(attestation)
        | OwnerJudgment::Foreign(attestation) => match close::repair_owner_is_live(attestation) {
            Ok(true) => Ok(()),
            Ok(false) => Err(owner_ended()),
            Err(error) => Err(Stop::unknown(
                "owner_unverified",
                Some(format!("{error:#}")),
            )),
        },
        OwnerJudgment::Unreadable(reason) => Err(Stop::unreadable(reason.clone())),
        OwnerJudgment::Absent => Ok(()),
    }
}

/// The owner side of `tell`'s surface ownership check, judged after the recorded handle as
/// the probe that verifies it runs after `tell` reads that handle. The probe requires an
/// owner bound to this session, so any other record is unattested. What the probe requires
/// of a bound owner is stated beside it (`SurfaceProbe::owner_requirement`); this gate only
/// maps that answer. An owner whose identity could not be inspected is unverified; a live
/// owner without the identity the probe compares is unknown. None of these is a death, so
/// none is refused; a death and an unreadable record are the liveness gate's verdicts.
fn owner_control_gate(
    owner: &OwnerJudgment,
    requirement: impl FnOnce(&OwnerAttestation) -> OwnerRequirement,
) -> std::result::Result<(), Stop> {
    match owner {
        OwnerJudgment::Bound(attestation) => match requirement(attestation) {
            OwnerRequirement::Met | OwnerRequirement::Gone => Ok(()),
            // The identity could not be read; its projection carries why.
            OwnerRequirement::Uninspected => Err(Stop::unknown(
                "owner_unverified",
                session::OwnerObservation::from(attestation).error,
            )),
            OwnerRequirement::ControlUnverified => {
                Err(Stop::unknown("owner_control_unverified", None))
            }
        },
        owner if owner.gone() => Ok(()),
        OwnerJudgment::Unreadable(_) => Ok(()),
        OwnerJudgment::UnboundLegacy(_) | OwnerJudgment::Foreign(_) | OwnerJudgment::Absent => {
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

/// Whether this session was configured to observe question emissions, and whether TAB
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
        provider::QuestionResponse::DeflectedToResult(reason) => {
            json!({"state": "deflected_to_result", "reason": reason, "answer_by": "tell"})
        }
        provider::QuestionResponse::AnswerAfterTurn(reason) => {
            json!({"state": "answer_after_turn", "reason": reason, "answer_by": "tell"})
        }
        provider::QuestionResponse::Unknown(detail) => {
            json!({"state": "unknown", "reason": "evidence_unreadable", "detail": detail})
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
    /// orders them, with the probe's requirement answering `answer`.
    fn gate(
        process: ProcessObservation,
        control: ControlAttestation,
        answer: OwnerRequirement,
    ) -> Value {
        let owner = OwnerJudgment::Bound(attestation(process, control, Some(true)));
        match owner_gate(&owner).and_then(|()| owner_control_gate(&owner, |_| answer)) {
            Ok(()) => json!("pass"),
            Err(stop) => stop.value(),
        }
    }

    fn unconsulted(_: &OwnerAttestation) -> OwnerRequirement {
        panic!("only a bound owner is judged by the probe's requirement")
    }

    fn unbound_and_foreign(attestation: fn() -> OwnerAttestation) -> [OwnerJudgment; 2] {
        [
            OwnerJudgment::UnboundLegacy(attestation()),
            OwnerJudgment::Foreign(attestation()),
        ]
    }

    // Every recorded owner is judged by Repair's liveness rule whatever Session it names; a
    // live record that is not bound to this session is judged by the ownership gate, which
    // grants it nothing. A death or an unreadable record is the liveness gate's verdict.
    #[test]
    fn the_liveness_gate_stops_only_where_repair_would() {
        let live = || {
            attestation(
                ProcessObservation::Live(Attested::Incarnation),
                ControlAttestation::NotRecorded,
                Some(true),
            )
        };
        let exited = || {
            attestation(
                ProcessObservation::Gone(Gone::Exited),
                ControlAttestation::NotRecorded,
                Some(false),
            )
        };
        let reused = || {
            attestation(
                ProcessObservation::Gone(Gone::Reused("birth".into())),
                ControlAttestation::NotRecorded,
                Some(true),
            )
        };
        let unknown = || {
            attestation(
                ProcessObservation::Unknown("birth denied".into()),
                ControlAttestation::NotRecorded,
                Some(false),
            )
        };
        let unattested = json!({"state": "unknown", "reason": "owner_unattested"});
        for owner in unbound_and_foreign(live)
            .into_iter()
            .chain([OwnerJudgment::Absent])
        {
            assert!(owner_gate(&owner).is_ok(), "{owner:?}");
            assert_eq!(
                owner_control_gate(&owner, unconsulted).unwrap_err().value(),
                unattested,
                "{owner:?}"
            );
        }
        for owner in unbound_and_foreign(exited)
            .into_iter()
            .chain(unbound_and_foreign(reused))
        {
            assert_eq!(
                owner_gate(&owner).unwrap_err().value()["reason"],
                "owner_ended",
                "{owner:?}"
            );
            assert!(owner_control_gate(&owner, unconsulted).is_ok(), "{owner:?}");
        }
        for owner in unbound_and_foreign(unknown) {
            assert_eq!(
                owner_gate(&owner).unwrap_err().value(),
                json!({"state": "unknown", "reason": "owner_unverified", "detail": "birth denied"}),
                "{owner:?}"
            );
            // Where Repair leaves the owner unexamined, the record still grants nothing.
            assert_eq!(
                owner_control_gate(&owner, unconsulted).unwrap_err().value(),
                unattested,
                "{owner:?}"
            );
        }
        let unreadable = OwnerJudgment::Unreadable("invalid JSON".into());
        assert_eq!(
            owner_gate(&unreadable).unwrap_err().value(),
            json!({"state": "unknown", "reason": "evidence_unreadable", "detail": "invalid JSON"})
        );
        assert!(owner_control_gate(&unreadable, unconsulted).is_ok());
    }

    /// What one side of the parity table decided about the owner.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Decision {
        /// Convergence goes on past the owner.
        Continues,
        /// The owner is dead: Repair closes the session, the prediction refuses.
        Ended,
        /// Neither can decide: Repair fails, the prediction is unknown.
        Fails,
    }

    /// One row of record shape and attestation answer, judged by `tell`'s prediction, by
    /// Repair and (on macOS) by the running wait, each with the same injected attestation.
    fn parity_row(shape: &str, answer: fn() -> OwnerAttestation) -> Vec<Decision> {
        use crate::native::session::{SessionState, Store, close, update_status};
        use crate::native::terminal::ownership::NativeSessionOwner;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-parity");
        std::fs::create_dir_all(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Exited, None, None).unwrap();
        let store = Store::open_unchecked(&directory);
        let owner = |binding: Option<&str>| NativeSessionOwner {
            pid: 4242,
            managed_session_id: binding.map(str::to_owned),
            ..NativeSessionOwner::default()
        };
        match shape {
            "bound" => store.write_owner(&owner(Some("session-parity"))).unwrap(),
            "unbound" => store.write_owner(&owner(None)).unwrap(),
            "foreign" => store.write_owner(&owner(Some("session-other"))).unwrap(),
            "absent" => (),
            "unreadable" => {
                std::fs::create_dir(store.record(session::CoreRecord::Owner).path()).unwrap()
            }
            _ => unreachable!("{shape}"),
        }
        let reader = Reader::open_unchecked(&directory);
        let evidence = crate::native::query::observation::SessionEvidence::read(&reader, Ok(false));
        let attest = |_: &NativeSessionOwner| answer();
        let predicted = match owner_gate(evidence.judge_owner_with(
            &crate::native::query::observation::judgment::Observers { attest: &attest },
        ))
        .map_err(|stop| stop.value())
        {
            Ok(()) => Decision::Continues,
            Err(stop) if stop["reason"] == "owner_ended" => Decision::Ended,
            Err(stop) => {
                assert_eq!(stop["state"], "unknown", "{shape}");
                Decision::Fails
            }
        };
        #[cfg(target_os = "macos")]
        let waited = match crate::native::session::turn::running_wait_owner_with(&directory, attest)
        {
            Ok(()) => Decision::Continues,
            Err(error) if format!("{error:#}").starts_with("native session process 4242") => {
                Decision::Ended
            }
            Err(_) => Decision::Fails,
        };
        let repaired = match close::repair_dead_owner_attested_with(&store, attest) {
            Ok(false) => Decision::Continues,
            Ok(true) => {
                assert_eq!(store.status().unwrap().state, SessionState::Closed);
                Decision::Ended
            }
            Err(_) => Decision::Fails,
        };
        vec![
            predicted,
            repaired,
            #[cfg(target_os = "macos")]
            waited,
        ]
    }

    // tell's prediction, Repair and the macOS running wait agree on every record shape and
    // every attestation answer, the foreign record with a dead process included.
    #[test]
    fn the_owner_prediction_agrees_with_repair_on_every_record_and_answer() {
        type Answer = fn() -> OwnerAttestation;
        let answers: [(&str, Answer); 4] = [
            ("live", || {
                attestation(
                    ProcessObservation::Live(Attested::Incarnation),
                    ControlAttestation::NotRecorded,
                    Some(true),
                )
            }),
            ("exited", || {
                attestation(
                    ProcessObservation::Gone(Gone::Exited),
                    ControlAttestation::NotRecorded,
                    Some(false),
                )
            }),
            ("reused", || {
                attestation(
                    ProcessObservation::Gone(Gone::Reused("birth".into())),
                    ControlAttestation::NotRecorded,
                    Some(true),
                )
            }),
            ("unknown", || {
                attestation(
                    ProcessObservation::Unknown("birth denied".into()),
                    ControlAttestation::NotRecorded,
                    Some(false),
                )
            }),
        ];
        for shape in ["bound", "unbound", "foreign", "absent", "unreadable"] {
            for (name, answer) in answers {
                let expected = match (shape, name) {
                    ("absent", _) => Decision::Continues,
                    ("unreadable", _) => Decision::Fails,
                    (_, "live") => Decision::Continues,
                    (_, "exited" | "reused") => Decision::Ended,
                    _ => Decision::Fails,
                };
                let decisions = parity_row(shape, answer);
                assert_eq!(decisions[..2], [expected, expected], "{shape} {name}");
                // The running wait ends on the same deaths; it keeps waiting on an owner
                // it cannot decide, where Repair fails.
                #[cfg(target_os = "macos")]
                assert_eq!(
                    decisions[2],
                    if shape == "unreadable" || expected != Decision::Fails {
                        expected
                    } else {
                        Decision::Continues
                    },
                    "{shape} {name}"
                );
            }
        }
    }

    // The gate maps the probe's answer for a bound owner and decides nothing of its own;
    // what each platform's probe answers for each attestation is tested beside the probe
    // (`terminal::ownership` tests). A death is the liveness gate's verdict, before this
    // gate, and passes this gate where repair leaves the owner unexamined.
    #[test]
    fn a_bound_owner_is_judged_by_the_probes_requirement_and_nothing_else() {
        let unverified =
            json!({"state": "unknown", "reason": "owner_unverified", "detail": "tty unreadable"});
        let control = json!({"state": "unknown", "reason": "owner_control_unverified"});
        let ended = json!({"state": "refused", "reason": "owner_ended",
            "detail": "the recorded session owner is no longer running"});
        let live = || ProcessObservation::Live(Attested::Incarnation);
        let rows = [
            (
                live(),
                ControlAttestation::Verified,
                OwnerRequirement::Met,
                json!("pass"),
            ),
            // The detail is the attestation's projected error, never the answer's.
            (
                live(),
                ControlAttestation::Unknown("tty unreadable".into()),
                OwnerRequirement::Uninspected,
                unverified,
            ),
            (
                live(),
                ControlAttestation::Changed("process group changed".into()),
                OwnerRequirement::ControlUnverified,
                control.clone(),
            ),
            (
                live(),
                ControlAttestation::Changed("process group changed".into()),
                OwnerRequirement::Met,
                json!("pass"),
            ),
            (
                live(),
                ControlAttestation::NotRecorded,
                OwnerRequirement::ControlUnverified,
                control.clone(),
            ),
            (
                live(),
                ControlAttestation::NotRecorded,
                OwnerRequirement::Met,
                json!("pass"),
            ),
            (
                ProcessObservation::Live(Attested::PidOnly),
                ControlAttestation::NotRecorded,
                OwnerRequirement::ControlUnverified,
                control,
            ),
            (
                ProcessObservation::Live(Attested::PidOnly),
                ControlAttestation::NotRecorded,
                OwnerRequirement::Met,
                json!("pass"),
            ),
            (
                ProcessObservation::Gone(Gone::Exited),
                ControlAttestation::NotRecorded,
                OwnerRequirement::Gone,
                ended.clone(),
            ),
            (
                ProcessObservation::Gone(Gone::Reused("another birth".into())),
                ControlAttestation::NotRecorded,
                OwnerRequirement::Gone,
                ended,
            ),
            (
                ProcessObservation::Unknown("access denied".into()),
                ControlAttestation::NotRecorded,
                OwnerRequirement::Uninspected,
                json!({"state": "unknown", "reason": "owner_unverified",
                    "detail": "access denied"}),
            ),
        ];
        for (process, control_attestation, answer, expected) in rows {
            let label = format!("{answer:?} {process:?} {control_attestation:?}");
            assert_eq!(
                gate(process, control_attestation, answer),
                expected,
                "{label}"
            );
        }
        for process in [Gone::Exited, Gone::Reused("another birth".into())] {
            let owner = OwnerJudgment::Bound(attestation(
                ProcessObservation::Gone(process),
                ControlAttestation::NotRecorded,
                Some(true),
            ));
            assert!(owner_control_gate(&owner, |_| OwnerRequirement::Gone).is_ok());
        }
    }
}
