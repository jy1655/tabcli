use super::*;
use crate::native::query::test_support::entries;
use crate::native::session::{Reader, Store, turn};
use crate::native::{doctor, session, tests::seed_close_fixture};

fn doctor_records(reader: &Reader) -> (Value, Value) {
    let mut checks = Vec::new();
    let mut observations = json!({});
    doctor::record_checks(
        reader,
        "session-fault",
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        &mut checks,
        &mut observations,
    );
    (serde_json::to_value(checks).unwrap(), observations)
}

fn reason<'a>(checks: &'a Value, id: &str) -> &'a str {
    checks
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["reason_code"]
        .as_str()
        .unwrap()
}

#[test]
fn damage_keeps_inspect_and_doctor_failure_scopes() {
    for part in [
        "status",
        "journal",
        "event",
        "resumed_from",
        "receipt",
        "owner",
        "surface",
    ] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let directory = &fixture.directory;
        // Retain the event as an ordinary published result to isolate its decode failure.
        session::turn::recover_pending_completion(directory).unwrap();
        // Keep the original observation fixture: a published result while the provider works.
        session::update_status(directory, SessionState::Claimed, None, None).unwrap();
        session::update_status(directory, SessionState::Working, None, None).unwrap();
        let reader = Reader::open_unchecked(directory);
        let initial = Observation::read(&reader).unwrap();
        let receipt = &initial.records.receipts[0];
        let path = match part {
            "status" => directory.join("status.json"),
            "journal" => directory.join("turn.completion.json"),
            "event" => directory.join("events").join(&receipt.event_file),
            "receipt" => directory
                .join("requests")
                .join(format!("{}.json", receipt.claim_token)),
            "owner" => directory.join("native-session.json"),
            "surface" => directory.join("terminal.json"),
            "resumed_from" => directory.join("manifest.json"),
            _ => unreachable!(),
        };
        if part == "resumed_from" {
            let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            manifest["resumed_from"] = json!({"session": 42});
            crate::native::write_json_atomic(&path, &manifest).unwrap();
        } else {
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            crate::native::write_private(&path, b"{").unwrap();
        }
        let before = entries(directory);
        let (observation, observation_reads) = with_event_read_count(|| Observation::read(&reader));
        let inspect = super::super::inspect_value(&reader, "session-fault");
        let ((checks, observed), doctor_reads) = with_event_read_count(|| doctor_records(&reader));
        if matches!(part, "status" | "journal") {
            assert!(observation.is_err(), "{part}");
            assert!(inspect.is_err(), "{part}");
            assert_eq!(reason(&checks, "session_records"), "records_unreadable");
            for id in ["session_state", "turn", "completion"] {
                assert_eq!(reason(&checks, id), "records_unreadable");
            }
            assert_eq!(observed, json!({}));
            assert_eq!(reason(&checks, "owner"), "owner_unverified");
            assert!(
                checks
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["id"] == "terminal_record")
            );
        } else {
            let observation = observation.unwrap();
            assert_eq!(reason(&checks, "session_records"), "records_read");
            assert_eq!(observed["stored_state"], "working");
            assert_eq!(observed["active_request_id"], Value::Null);
            assert_eq!(observed["recovery_required"], false);
            match part {
                "event" => {
                    assert_eq!(observation_reads, 0);
                    assert_eq!(doctor_reads, 0);
                    assert!(ResultReference::latest(&observation.records, &reader).is_err());
                    assert!(inspect.is_err());
                }
                "resumed_from" => {
                    assert!(observation.resumed_from.is_err());
                    assert!(inspect.is_err());
                }
                "receipt" => {
                    assert_eq!(observation.records.unreadable_requests, 1);
                    assert_eq!(reason(&checks, "request_index"), "request_index_incomplete");
                    let inspect = inspect.unwrap();
                    assert_eq!(inspect["unreadable_requests"], 1);
                    assert_eq!(inspect["requests"], json!([]));
                    assert_eq!(inspect["latest_result"]["request_state"], "completed");
                }
                "owner" => {
                    assert!(observation.evidence.owner.is_err());
                    assert!(inspect.is_ok());
                    assert_eq!(reason(&checks, "owner"), "owner_unreadable");
                }
                "surface" => {
                    assert!(matches!(
                        observation.evidence.surface,
                        SurfaceRecord::Active(Err(_))
                    ));
                    assert!(inspect.is_ok());
                    assert_eq!(
                        reason(&checks, "terminal_record"),
                        "terminal_record_unreadable"
                    );
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(entries(directory), before, "{part}");
    }
}

#[test]
fn observation_is_read_only_and_releases_its_lock() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    crate::native::write_json_atomic(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":0, "managed_session_id":"session-fault"}),
    )
    .unwrap();
    // The crate-internal snapshot must also retain empty directories.
    let original = entries(&fixture.directory);
    let empty = fixture.directory.join("empty");
    fs::create_dir(&empty).unwrap();
    let with_empty = entries(&fixture.directory);
    assert_ne!(with_empty, original);
    assert!(with_empty.contains(&(empty.clone(), None)));
    fs::remove_dir(&empty).unwrap();
    assert_ne!(entries(&fixture.directory), with_empty);
    assert_eq!(entries(&fixture.directory), original);
    let before = entries(&fixture.directory);
    for owner in [false, true] {
        let observation = Observation::read(&reader).unwrap();
        assert!(observation.evidence.owner_judgment.get().is_none());
        if owner {
            observation.evidence.judge_owner("session-fault");
            assert!(observation.evidence.owner_judgment.get().unwrap().gone());
        }
        assert!(observation.evidence.surface_presence.get().is_none());
        // Compare bytes before taking the writer lock: Windows refuses to read a file
        // another handle holds exclusively, and the lock file is part of the directory.
        assert_eq!(entries(&fixture.directory), before);
        let writer = File::open(fixture.directory.join("turn.claim.lock")).unwrap();
        writer.try_lock().unwrap();
        drop(writer);
    }
    fs::remove_file(fixture.directory.join("turn.claim.lock")).unwrap();
    let before = entries(&fixture.directory);
    Observation::read(&reader).unwrap();
    assert_eq!(entries(&fixture.directory), before);
}

#[test]
fn unlocked_turn_facts_cannot_guard_later_adapter_reads() {
    // Q8: retained facts cannot detect release or replacement between their read and
    // the adapter's log read. Agy must keep its existing shared lock over both reads.
    for replace in [false, true] {
        let fixture = seed_close_fixture(JournaledEventState::Absent);
        let directory = &fixture.directory;
        fs::remove_file(directory.join("turn.completion.json")).unwrap();
        let reader = Reader::open_unchecked(directory);
        let facts = Observation::read(&reader).unwrap();
        let pending_token = facts.records.claim.clone().unwrap();
        assert_eq!(facts.records.status.state, SessionState::Working);
        assert!(facts.records.status.error.is_none());
        assert!(facts.records.pending.is_none());
        assert!(facts.active_request().is_some());

        // A real lifecycle writer can finish before the adapter examines its evidence.
        session::turn::Report::for_claim(
            &session::Store::open_unchecked(directory),
            agent_bridge::FirstPartyCli::Codex,
            Some(&pending_token),
        )
        .complete("completed", None, None)
        .unwrap();
        if replace {
            turn::claim(&Store::open_unchecked(directory), &[])
                .unwrap()
                .retain();
        }
        assert_eq!(facts.records.claim.as_deref(), Some(pending_token.as_str()));
        let current = observe_snapshot(&reader).unwrap();
        assert_ne!(current.claim.as_deref(), Some(pending_token.as_str()));
        // The existing adapter therefore takes its None path for the old pending token.
        // While it reads the pending record, log and transcript, a writer cannot pass.
        let writer = File::open(directory.join("turn.claim.lock")).unwrap();
        assert!(matches!(
            writer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(current);
        writer.try_lock().unwrap();
    }
}

#[test]
fn inspect_rejects_provenance_before_reading_latest_event() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let directory = &fixture.directory;
    session::turn::recover_pending_completion(directory).unwrap();
    let reader = Reader::open_unchecked(directory);
    let event_path = reader.events().unwrap().remove(0);
    fs::write(&event_path, b"{").unwrap();
    let manifest_path = directory.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["resumed_from"] = json!({"session": 42});
    crate::native::write_json_atomic(&manifest_path, &manifest).unwrap();
    let expected_error = format!("{:#}", read_resumed_from(directory).unwrap_err());
    let (inspect, reads) =
        with_event_read_count(|| super::super::inspect_value(&reader, "session-fault"));
    assert_eq!(format!("{:#}", inspect.unwrap_err()), expected_error);
    assert_eq!(reads, 0);

    manifest.as_object_mut().unwrap().remove("resumed_from");
    crate::native::write_json_atomic(&manifest_path, &manifest).unwrap();
    let (inspect, reads) =
        with_event_read_count(|| super::super::inspect_value(&reader, "session-fault"));
    assert!(inspect.is_err());
    assert_eq!(reads, 1);
}

#[test]
fn an_active_result_damaged_after_publication_is_an_unreadable_active_state() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let event_path = reader.events().unwrap().remove(0);
    // Damage the active event after its publication comparison, while retaining the
    // claim and valid journal. This isolates the active Request's decode error from the
    // turn judgment, which still sees the claim.
    let damaged = event_path.clone();
    with_snapshot_hook(
        move |_| fs::write(&damaged, b"{").unwrap(),
        || {
            let observation = Observation::read(&reader).unwrap();
            assert!(observation.records.claim.is_some());
            assert!(
                observation
                    .judgments
                    .active_state
                    .as_ref()
                    .unwrap()
                    .is_err()
            );
            assert_eq!(observation.judgments.turn, judgment::Turn::TurnInProgress);
            assert!(!observation.judgments.claim_without_receipt);
        },
    );
}

#[test]
fn each_damaged_auxiliary_record_is_its_unreadable_part() {
    use judgment::UnreadablePart;
    for (part, expected) in [
        ("owner", UnreadablePart::Owner),
        ("surface", UnreadablePart::SurfaceRecord),
        ("tombstone", UnreadablePart::UnconsumedCloseTombstone),
        ("hold", UnreadablePart::Hold),
        ("resumed_from", UnreadablePart::ResumedFrom),
        ("receipt", UnreadablePart::RequestIndex),
        ("request_index", UnreadablePart::RequestIndex),
    ] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let directory = &fixture.directory;
        session::turn::recover_pending_completion(directory).unwrap();
        session::update_status(directory, SessionState::Claimed, None, None).unwrap();
        session::update_status(directory, SessionState::Working, None, None).unwrap();
        let reader = Reader::open_unchecked(directory);
        let observation = Observation::read(&reader).unwrap();
        assert!(observation.judgments.unreadable.is_empty(), "{part}");
        let receipt = &observation.records.receipts[0];
        let path = match part {
            "owner" => directory.join("native-session.json"),
            "surface" => directory.join("terminal.json"),
            "tombstone" => directory.join("terminal.closed.json"),
            "hold" => directory.join("hold.json"),
            "resumed_from" => directory.join("manifest.json"),
            "receipt" => directory
                .join("requests")
                .join(format!("{}.json", receipt.claim_token)),
            "request_index" => directory.join("requests"),
            _ => unreachable!(),
        };
        match part {
            "resumed_from" => {
                let mut manifest: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                manifest["resumed_from"] = json!({"session": 42});
                crate::native::write_json_atomic(&path, &manifest).unwrap();
            }
            "tombstone" => {
                crate::native::write_json_atomic(&path, &json!({"terminal": "iterm"})).unwrap()
            }
            _ => {
                if path.is_dir() {
                    fs::remove_dir_all(&path).unwrap();
                } else if path.exists() {
                    fs::remove_file(&path).unwrap();
                }
                crate::native::write_private(&path, b"{").unwrap();
            }
        }
        let observation = Observation::read(&reader).unwrap();
        assert_eq!(observation.judgments.unreadable, vec![expected], "{part}");
        assert_eq!(observation.records.status.state, SessionState::Working);
        if part == "tombstone" {
            assert_eq!(
                observation.judgments.surface,
                judgment::Surface::Closed { consumed: false }
            );
        }
    }
}

#[test]
fn surface_presence_is_judged_once_per_observation() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    crate::native::write_json_atomic(
        &fixture.directory.join("terminal.json"),
        &json!({"terminal": "warp", "session_id": "surface-1", "managed_session_id": "session-fault"}),
    )
    .unwrap();
    let reader = Reader::open_unchecked(&fixture.directory);
    let observation = Observation::read(&reader).unwrap();
    assert!(observation.evidence.surface_presence.get().is_none());
    let calls = std::cell::Cell::new(0);
    let probe = |directory: &Path, handle: &terminal::TerminalSession, budget: Duration| {
        calls.set(calls.get() + 1);
        assert_eq!(directory, fixture.directory.as_path());
        assert_eq!(handle.id, "surface-1");
        assert!(budget <= Duration::from_secs(2));
        Ok(false)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..3 {
        let presence = observation
            .evidence
            .judge_surface_presence_with(&reader, deadline, &probe);
        assert_eq!(*presence, judgment::SurfacePresence::Missing);
    }
    assert_eq!(calls.get(), 1);
    // The memoized judgment is the one every renderer of this Observation reads.
    assert_eq!(
        *observation
            .evidence
            .judge_surface_presence(&reader, deadline),
        judgment::SurfacePresence::Missing
    );
    assert_eq!(calls.get(), 1);
    // Without a bound active handle the probe is never asked (D18: the current read).
    let unbound = Observation::read(&reader).unwrap();
    fs::remove_file(fixture.directory.join("terminal.json")).unwrap();
    let presence = unbound
        .evidence
        .judge_surface_presence_with(&reader, deadline, &|_, _, _| panic!("no handle to probe"));
    assert_eq!(
        *presence,
        judgment::SurfacePresence::Unverified("no active terminal handle is recorded".into())
    );
    let expired = Observation::read(&reader).unwrap();
    crate::native::write_json_atomic(
        &fixture.directory.join("terminal.json"),
        &json!({"terminal": "warp", "session_id": "surface-1", "managed_session_id": "session-fault"}),
    )
    .unwrap();
    let presence =
        expired
            .evidence
            .judge_surface_presence_with(&reader, Instant::now(), &|_, _, _| {
                panic!("no budget to probe")
            });
    assert_eq!(
        *presence,
        judgment::SurfacePresence::Unverified("diagnostic probe deadline exhausted".into())
    );
}

#[test]
fn inspect_holds_the_lifecycle_lock_through_its_historical_elapsed_reads() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let directory = &fixture.directory;
    // A second, historical request beside the active one.
    crate::native::write_json_atomic(
        &directory.join("events/event-000.json"),
        &json!({"provider":"codex","message":"earlier result","error":null,
            "provider_session_id":null,"turn_id":null,"created_unix_ms":1}),
    )
    .unwrap();
    crate::native::write_json_atomic(
        &directory.join("requests/1-1-1.json"),
        &json!({"schema":1,"request_id":"request-earlier","claim_token":"1-1-1",
            "event_file":"event-000.json","created_unix_ms":0}),
    )
    .unwrap();
    let reader = Reader::open_unchecked(directory);
    let receipts = observe_snapshot(&reader).unwrap().receipts.len();
    assert_eq!(receipts, 2);
    let lock_path = directory.join("turn.claim.lock");
    let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = held.clone();
    let inspect = with_event_read_hook(
        move |_| {
            let writer = File::open(&lock_path).unwrap();
            seen.lock().unwrap().push(matches!(
                writer.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        },
        || super::super::inspect_value(&reader, "session-fault").unwrap(),
    );
    // The active Request's state, the latest Result and one historical elapsed read per
    // receipt, every decode under the lock.
    assert_eq!(*held.lock().unwrap(), vec![true; receipts + 2]);
    assert_eq!(inspect["requests"].as_array().unwrap().len(), receipts);
    File::open(directory.join("turn.claim.lock"))
        .unwrap()
        .try_lock()
        .unwrap();
}

#[test]
fn result_reference_omits_only_the_body_key() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let observation = Observation::read_locked(&reader).unwrap();
    let mut result = observation
        .records
        .result(&reader, &Selector::Latest)
        .unwrap();
    assert_eq!(
        result.as_object_mut().unwrap().remove("result"),
        Some(json!("late result"))
    );
    let reference = ResultReference::latest(&observation.records, &reader)
        .unwrap()
        .value();
    assert!(reference.get("result").is_none());
    assert_eq!(reference, result);
}

#[test]
fn doctor_reads_the_origin_within_its_own_deadline() {
    use crate::native::session::{LAUNCH_FILE, REQUESTS_DIRECTORY, TURN_CLAIM_FILE};
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let directory = &fixture.directory;
    // The initial request names a managed origin, so observing it reads its records.
    let token = fs::read_to_string(directory.join(TURN_CLAIM_FILE)).unwrap();
    let token = token.trim();
    crate::native::write_json_atomic(
        &directory.join(LAUNCH_FILE),
        &json!({"schema": 1, "claim_token": token, "deadline_unix_ms": 1, "phase": "spawned"}),
    )
    .unwrap();
    let receipt_path = directory
        .join(REQUESTS_DIRECTORY)
        .join(format!("{token}.json"));
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    receipt["source"] = json!("session-origin");
    crate::native::write_json_atomic(&receipt_path, &receipt).unwrap();
    // The origin's directory exists, so observing it reads its manifest.
    fs::create_dir(directory.parent().unwrap().join("session-origin")).unwrap();
    let reader = Reader::open_unchecked(directory);
    let run = |deadline| {
        let mut checks = Vec::new();
        let mut observations = json!({});
        doctor::record_checks(
            &reader,
            "session-fault",
            deadline,
            &mut checks,
            &mut observations,
        );
        observations
    };
    // With time left its records are read: it has no manifest.
    let observations = run(std::time::Instant::now() + std::time::Duration::from_secs(5));
    assert_eq!(observations["origin"], "session-origin");
    assert_eq!(
        observations["origin_observation"]["reason"],
        "origin_unreadable"
    );
    // An expired doctor deadline reads nothing of the origin.
    let observations = run(std::time::Instant::now());
    assert_eq!(
        observations["origin_observation"]["reason"],
        "origin_budget_exhausted"
    );
    assert!(observations["origin_observation"]["ended"].is_null());
}

#[test]
fn owner_judgment_is_made_once_per_observation() {
    use crate::native::process_identity::{Attested, ProcessObservation};
    use crate::native::session::owner::ControlAttestation;
    use judgment::tests::attestation;
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    crate::native::write_json_atomic(
        &fixture.directory.join("native-session.json"),
        &json!({"pid": 4242, "managed_session_id": "session-fault"}),
    )
    .unwrap();
    let observation = Observation::read(&Reader::open_unchecked(&fixture.directory)).unwrap();
    let calls = std::cell::Cell::new(0);
    let observers = judgment::Observers {
        attest: &|owner| {
            calls.set(calls.get() + 1);
            assert_eq!(owner.pid, 4242);
            attestation(
                ProcessObservation::Live(Attested::PidOnly),
                ControlAttestation::NotRecorded,
                Some(true),
            )
        },
        pid_alive: &|_| panic!("a bound record is attested"),
    };
    for _ in 0..3 {
        let owner = observation
            .evidence
            .judge_owner_with("session-fault", &observers);
        assert_eq!(owner.observation().process_alive, Some(true));
    }
    assert_eq!(calls.get(), 1);
    // The memoized judgment is the one every renderer of this Observation reads.
    assert_eq!(
        observation
            .evidence
            .judge_owner("session-fault")
            .observation()
            .process_alive,
        Some(true)
    );
    assert_eq!(calls.get(), 1);
}
