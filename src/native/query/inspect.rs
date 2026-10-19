//! The inspect renderer: one locked Observation of a session, its records and judgments.
use super::*;
use crate::native::clock::SystemClock;
use crate::native::session;
use crate::native::session::origin::OriginEnvironment;
use observation::{Observation, ResultReference};

pub(super) fn inspect_inner(id: &str, json: bool) -> Result<()> {
    let directory = Reader::session_directory(id)?;
    let value = inspect_value(&Reader::open_unchecked(&directory), id)?;
    envelope::emit(json, &value, || {
        println!(
            "session: {id}\nprovider: {}\nstate (stored): {}\nworkspace: {}\nupdated: {}",
            value["provider"].as_str().unwrap_or("?"),
            terminal_safe_text(value["stored_state"].as_str().unwrap_or("unknown"), false),
            terminal_safe_text(value["workspace"].as_str().unwrap_or("?"), false),
            value["updated_unix_ms"]
        );
        println!(
            "owner process alive: {}\nrecovery required: {}",
            value["owner_process_alive"], value["recovery_required"]
        );
        println!("held: {}", value["held"]);
        if let Some(error) = value["hold_error"].as_str() {
            println!("hold error: {}", terminal_safe_text(error, true));
        }
        if let Some(source) = value["resumed_from"]["session"].as_str() {
            println!("resumed from: {}", terminal_safe_text(source, false));
        }
        if let Some(error) = value["error"].as_str() {
            println!("error: {}", terminal_safe_text(error, true));
        }
        if let Some(id) = value["latest_result"]["event_id"].as_str() {
            println!("latest result: {id}");
        }
        for request in value["requests"].as_array().into_iter().flatten() {
            println!("request: {}", request["request_id"].as_str().unwrap_or("?"));
        }
        Ok(())
    })
}

/// Reads the Observation with its lifecycle lock and keeps the lock through the latest
/// Result and every historical elapsed read (D18).
pub(in crate::native) fn inspect_value(reader: &Reader, id: &str) -> Result<Value> {
    let mut clock = SystemClock;
    value(
        Observation::read_locked(reader)?,
        reader,
        id,
        &mut OriginEnvironment::production(&mut clock),
    )
}

/// Renders a locked Observation. A bad `resumed_from` is rejected before the latest
/// Result is read; the Snapshot lock is held until the value is complete.
pub(super) fn value(
    observation: Observation,
    reader: &Reader,
    id: &str,
    environment: &mut OriginEnvironment<'_>,
) -> Result<Value> {
    let origin = observation.observe_origin(reader, None, environment);
    let owner = observation.evidence.judge_owner(id).observation();
    let judgments = &observation.judgments;
    let snapshot = &observation.records;
    let resumed_from = observation.resumed_from?;
    let latest = ResultReference::latest(snapshot, reader)?.value();
    let request_refs = snapshot
        .receipts
        .iter()
        .map(|receipt| {
            let (elapsed, elapsed_reason) = match snapshot.event(reader, &receipt.event_file) {
                Ok(event) => observed_elapsed(Some(receipt), event.as_ref()),
                Err(_) => (None, Some("unreadable_result")),
            };
            let mut value = json!({
                "request_id": receipt.request_id, "created_unix_ms": receipt.created_unix_ms, "source": receipt.source,
                "event_id": receipt.event_file, "context_sources": receipt.context_sources,
                "active": snapshot.is_active(receipt),
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
            });
            if let Some(questions) = question_emissions(reader, snapshot, receipt) {
                question_presence(&mut value, &questions);
            }
            value
        })
        .collect::<Vec<_>>();
    let mut value = envelope::ok(json!({
        "session": id, "provider": snapshot.manifest.provider,
        "workspace": snapshot.manifest.workspace, "title": snapshot.manifest.title,
        "stored_state": snapshot.status.state, "generation": snapshot.status.generation,
        "created_unix_ms": snapshot.manifest.created_unix_ms, "updated_unix_ms": snapshot.status.updated_unix_ms,
        "error": snapshot.status.error, "exit_code": snapshot.status.exit_code,
        "configured": {"model": snapshot.manifest.model, "effort": snapshot.manifest.effort,
            "yolo": snapshot.manifest.yolo, "provider_version_at_launch": snapshot.manifest.provider_version},
        "resumed_from": resumed_from,
        "workspace_consent": observation.workspace_consent.into_value(),
        "cancel": observation.cancel,
        "owner_process_alive": owner.process_alive, "owner_identity_verified": owner.identity_matches == Some(true), "owner": owner,
        "recovery_required": judgments.recovery_required, "turn_claimed": snapshot.claim.is_some(),
        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
        "recorded_events": snapshot.paths.len(), "latest_result": latest, "requests": request_refs,
    }).as_object().unwrap().clone());
    session::hold::add_fields(&mut value, &observation.evidence.held);
    if let Some(residual) = judgments.residual_surface {
        value["residual_surface"] = serde_json::to_value(residual)?;
    }
    origin.add_fields(&mut value);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::process_identity::{Attested, Gone, ProcessObservation};
    use crate::native::query::observation::judgment::tests::attestation;
    use crate::native::query::observation::judgment::{self, OwnerJudgment};
    use crate::native::session::launch::ResidualSurface;
    use crate::native::session::owner::ControlAttestation::{NotRecorded, Verified};
    use crate::native::session::owner::OwnerAttestation;
    use crate::native::tests::seed_close_fixture;

    #[test]
    fn inspect_maps_its_judgments_and_the_owner() {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        crate::native::session::primitives::write_json_atomic(
            &fixture.directory.join("native-session.json"),
            &json!({"pid": 4242, "managed_session_id": "session-fault"}),
        )
        .unwrap();
        type Row = (
            bool,
            Option<ResidualSurface>,
            fn() -> OwnerAttestation,
            Value,
            bool,
        );
        let rows: [Row; 2] = [
            (
                true,
                Some(ResidualSurface::Unverified),
                || {
                    attestation(
                        ProcessObservation::Gone(Gone::Exited),
                        NotRecorded,
                        Some(false),
                    )
                },
                json!(false),
                false,
            ),
            (
                false,
                None,
                || {
                    attestation(
                        ProcessObservation::Live(Attested::Incarnation),
                        Verified,
                        Some(true),
                    )
                },
                json!(true),
                true,
            ),
        ];
        for (recovery, residual, attested, expected_alive, verified) in rows {
            let mut observation = Observation::read_locked(&reader).unwrap();
            observation.judgments.recovery_required = recovery;
            observation.judgments.residual_surface = residual;
            let owner = observation.evidence.judge_owner_with(
                "session-fault",
                &judgment::Observers {
                    attest: &|_| attested(),
                    pid_alive: &|_| panic!("a bound record is attested"),
                },
            );
            assert!(matches!(owner, OwnerJudgment::Bound(_)));
            let mut clock = SystemClock;
            let value = value(
                observation,
                &reader,
                "session-fault",
                &mut OriginEnvironment::production(&mut clock),
            )
            .unwrap();
            assert_eq!(value["recovery_required"], recovery);
            assert_eq!(
                value.get("residual_surface").cloned(),
                residual.map(|_| json!("unverified"))
            );
            assert_eq!(value["owner_process_alive"], expected_alive);
            assert_eq!(value["owner_identity_verified"], verified);
            assert_eq!(value["owner"]["process_alive"], expected_alive);
            assert_eq!(value["held"], false);
            assert_eq!(value["requests"].as_array().unwrap().len(), 1);
        }
    }
}
