use super::*;
use crate::native::{session, tests::seed_close_fixture, write_json_atomic, write_private};

fn read(reader: &Reader) -> StatusObservation {
    Observation::read_for_status(reader, Instant::now() + SEARCH_TIME_BUDGET, true)
        .unwrap()
        .unwrap()
}

fn flag(observed: &StatusObservation, expected: &str) -> bool {
    attention(observed, &OwnerObservation::default(), false).contains(&expected)
}

#[test]
fn attention_uses_recorded_delivery_launch_claim_and_state_facts() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut observed = read(&reader);
    assert!(flag(&observed, "recovery_required"));
    assert!(!flag(&observed, "delivery_unconfirmed"));
    observed.observation.records.pending = None;
    observed.observation.records.status.error = Some("delivery uncertain".to_owned());
    assert!(flag(&observed, "delivery_unconfirmed"));
    assert!(!flag(&observed, "claim_without_receipt"));
    observed.observation.records.receipts.clear();
    assert!(flag(&observed, "claim_without_receipt"));
    for (state, phase, reason) in [
        (
            SessionState::Launching,
            launch::Phase::Pending,
            "launch_timeout",
        ),
        (
            SessionState::Failed,
            launch::Phase::Pending,
            "launch_failed",
        ),
        (
            SessionState::Failed,
            launch::Phase::Spawning,
            "launch_uncertain",
        ),
    ] {
        let records = &mut observed.observation.records;
        records.status.state = state;
        records.launch = Some(launch::Record {
            schema: 1,
            claim_token: records.claim.clone().unwrap(),
            deadline_unix_ms: 1,
            phase,
        });
        observed.observation.judgments.launch_failure = launch::diagnostic(
            records.launch.as_ref(),
            &records.status,
            records.claim.as_deref(),
        );
        assert!(flag(&observed, reason));
    }
    assert!(flag(&observed, "session_failed"));
    observed.observation.records.status.state = SessionState::Exited;
    assert!(flag(&observed, "session_exited"));
    observed.observation.records.status.residual_surface =
        Some(launch::ResidualSurface::Unverified);
    assert!(flag(&observed, "residual_surface_unverified"));
    observed.observation.records.status.residual_surface = Some(launch::ResidualSurface::Cleared);
    assert!(!flag(&observed, "residual_surface_unverified"));
}

#[test]
fn owner_attention_distinguishes_negative_and_missing_facts_including_linux() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut observed = read(&reader);
    for (alive, identity, bound, expected) in [
        (Some(false), None, true, vec!["owner_exited"]),
        (
            Some(true),
            Some(false),
            true,
            vec!["owner_identity_mismatch"],
        ),
        (None, None, true, vec!["owner_unverified"]),
        (Some(true), None, true, vec![]),
        (Some(true), Some(true), true, vec![]),
        (None, None, false, vec!["owner_unverified"]),
    ] {
        let owner = OwnerObservation {
            process_alive: alive,
            identity_matches: identity,
            error: None,
        };
        let actual = attention(&observed, &owner, bound)
            .into_iter()
            .filter(|flag| flag.starts_with("owner_"))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
    observed.observation.records.status.state = SessionState::Closed;
    assert!(!flag(&observed, "owner_unverified"));
    let owner = OwnerObservation {
        process_alive: Some(false),
        identity_matches: Some(false),
        error: None,
    };
    assert!(
        !attention(&observed, &owner, true)
            .iter()
            .any(|f| f.starts_with("owner_"))
    );
}

#[test]
fn a_live_owner_identity_check_error_requires_attention() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let reader = Reader::open_unchecked(&fixture.directory);
    let observed = read(&reader);
    let owner = OwnerObservation {
        process_alive: Some(true),
        identity_matches: None,
        error: Some("identity check failed".to_owned()),
    };
    let flags = attention(&observed, &owner, true)
        .into_iter()
        .filter(|flag| flag.starts_with("owner_"))
        .collect::<Vec<_>>();
    assert_eq!(flags, ["owner_unverified"]);
}

#[test]
fn each_auxiliary_failure_keeps_the_session_with_attention() {
    for part in [
        "owner",
        "surface",
        "resumed_from",
        "event",
        "receipt",
        "request_index",
    ] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let directory = &fixture.directory;
        fs::remove_file(directory.join("turn.completion.json")).unwrap();
        session::turn::release_turn_claim(directory).unwrap();
        let reader = Reader::open_unchecked(directory);
        let observed = read(&reader);
        let receipt = &observed.observation.records.receipts[0];
        let path = match part {
            "owner" => directory.join("native-session.json"),
            "surface" => directory.join("terminal.json"),
            "resumed_from" => directory.join("manifest.json"),
            "event" => directory.join("events").join(&receipt.event_file),
            "receipt" => directory
                .join("requests")
                .join(format!("{}.json", receipt.claim_token)),
            "request_index" => directory.join("requests"),
            _ => unreachable!(),
        };
        if part == "resumed_from" {
            let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            manifest["resumed_from"] = json!({"session": 42});
            write_json_atomic(&path, &manifest).unwrap();
        } else {
            if path.is_dir() {
                fs::remove_dir_all(&path).unwrap();
            } else if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            write_private(&path, b"{").unwrap();
        }
        let observed = read(&reader);
        assert!(flag(&observed, "records_partially_unreadable"), "{part}");
        let value = entry(observed, "session-fault");
        assert_eq!(value["state"], "working");
        if part == "event" {
            assert!(value["latest_result"].is_null());
        }
        if matches!(part, "receipt" | "request_index") {
            assert!(
                value["attention"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("request_index_incomplete"))
            );
        }
    }
}

#[test]
fn result_commands_use_active_then_claim_then_exact_event_then_null() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut observed = read(&reader);
    let request = observed
        .observation
        .active_request()
        .unwrap()
        .request_id
        .clone();
    assert_eq!(
        result_command(&observed, "session-fault"),
        Some(format!(
            "{PUBLIC_COMMAND} result session-fault --request {request} --json"
        ))
    );
    observed.observation.records.receipts.clear();
    assert_eq!(
        result_command(&observed, "session-fault"),
        Some(format!(
            "{PUBLIC_COMMAND} result session-fault --list --json"
        ))
    );
    observed.observation.records.claim = None;
    let event = observed.latest.as_ref().unwrap()["event_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        result_command(&observed, "session-fault"),
        Some(format!(
            "{PUBLIC_COMMAND} result session-fault --event {event} --json"
        ))
    );
    observed.latest = Ok(Value::Null);
    assert_eq!(result_command(&observed, "session-fault"), None);
}

#[test]
fn status_decodes_only_latest_and_reuses_it_for_active_elapsed() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    write_private(&fixture.directory.join("events/event-000.json"), b"{").unwrap();
    let (observed, reads) = with_event_read_count(|| read(&reader));
    assert_eq!(reads, 1);
    assert!(!flag(&observed, "records_partially_unreadable"));
    assert!(
        observed
            .active
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .get("result")
            .is_none()
    );
    assert!(observed.latest.as_ref().unwrap().get("result").is_none());
    let value = entry(observed, "session-fault");
    assert_eq!(value["active_request"]["request_state"], "completed");
    assert_eq!(value["latest_result"]["request_state"], "completed");
    assert_eq!(
        value["active_request"]["bridge_observed_elapsed_reason"],
        "inverted_time"
    );
    fs::remove_file(fixture.directory.join("turn.completion.json")).unwrap();
    let event = reader
        .events()
        .unwrap()
        .into_iter()
        .find(|p| !p.ends_with("event-000.json"))
        .unwrap();
    fs::remove_file(event).unwrap();
    fs::remove_file(fixture.directory.join("events/event-000.json")).unwrap();
    let value = entry(read(&reader), "session-fault");
    assert_eq!(value["active_request"]["request_state"], "pending");
    assert!(value["active_request"]["bridge_observed_elapsed_ms"].is_null());
    assert_eq!(
        value["active_request"]["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
}

fn all() -> StatusRequest {
    parse_options(&["--all-workspaces".to_owned()]).unwrap()
}

#[test]
fn scanned_counts_attempts_after_scope_and_closed_filter_ignores_tombstone() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let root = fixture.directory.parent().unwrap();
    write_json_atomic(&fixture.directory.join("closed.json"),
        &json!({"state":"closed","generation":99,"updated_unix_ms":99,"error":null,"exit_code":null})).unwrap();
    let value = value_in(root, &all(), SEARCH_TIME_BUDGET).unwrap();
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":1}));
    assert_eq!(value["sessions"][0]["state"], "working");
    let mut request = all();
    request.scope = SearchScope::Workspace(root.join("different"));
    let lock = File::open(fixture.directory.join("turn.claim.lock")).unwrap();
    lock.lock().unwrap();
    let value = value_in(root, &request, SEARCH_TIME_BUDGET).unwrap();
    assert_eq!(value["scanned"], json!({"sessions":0,"listed":0}));
    assert_eq!(value["incomplete"], false);
    lock.unlock().unwrap();
    fs::remove_file(fixture.directory.join("status.json")).unwrap();
    write_private(&fixture.directory.join("status.json"), b"{").unwrap();
    let value = value_in(root, &all(), SEARCH_TIME_BUDGET).unwrap();
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
    assert_eq!(value["incomplete_reasons"][0]["session"], "session-fault");
}

#[test]
fn many_busy_sessions_share_remaining_budget_and_each_gets_a_reason() {
    let root = tempfile::tempdir().unwrap();
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("manifest.json")).unwrap())
            .unwrap();
    let mut locks = Vec::new();
    for index in 0..8 {
        let directory = root.path().join(format!("session-busy{index}"));
        fs::create_dir(&directory).unwrap();
        manifest["id"] = json!(format!("session-busy{index}"));
        write_json_atomic(&directory.join("manifest.json"), &manifest).unwrap();
        let lock = File::create(directory.join("turn.claim.lock")).unwrap();
        lock.lock().unwrap();
        locks.push(lock);
    }
    let started = Instant::now();
    let value = with_snapshot_retry_window(Duration::from_secs(5), || {
        value_in(root.path(), &all(), Duration::from_millis(60))
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
    let reasons = value["incomplete_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 8);
    assert!(reasons[0]["reason"].as_str().unwrap().contains("busy"));
    for (index, reason) in reasons.iter().enumerate().skip(1) {
        assert_eq!(reason["session"], format!("session-busy{index}"));
        assert!(
            reason["reason"]
                .as_str()
                .unwrap()
                .contains("budget exhausted")
        );
    }
}

#[test]
fn a_manifest_that_disappears_after_scope_is_an_incomplete_observation() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let path = fixture.directory.join("manifest.json");
    let value = with_snapshot_hook(
        move |_| {
            let _ = fs::remove_file(&path);
        },
        || {
            value_in(
                fixture.directory.parent().unwrap(),
                &all(),
                SEARCH_TIME_BUDGET,
            )
        },
    )
    .unwrap();
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
    assert_eq!(value["incomplete_reasons"][0]["session"], "session-fault");
}

#[test]
fn closed_filter_precedes_event_decode_and_tombstone_shape_stays_distinct() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut status: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("status.json")).unwrap()).unwrap();
    status["state"] = json!("closed");
    write_json_atomic(&fixture.directory.join("status.json"), &status).unwrap();
    let (observed, reads) = with_event_read_count(|| {
        Observation::read_for_status(&reader, Instant::now() + SEARCH_TIME_BUDGET, false).unwrap()
    });
    assert!(observed.is_none());
    assert_eq!(reads, 0);
    for (tombstone, partial) in [
        (json!({"consumed":true,"terminal":"iterm"}), false),
        (json!(42), true),
    ] {
        write_json_atomic(&fixture.directory.join("terminal.closed.json"), &tombstone).unwrap();
        assert_eq!(
            flag(&read(&reader), "records_partially_unreadable"),
            partial
        );
    }
}

#[test]
fn owner_binding_is_required_before_attestation_and_record_failures_stay_null() {
    let fixture = seed_close_fixture(JournaledEventState::Absent);
    let reader = Reader::open_unchecked(&fixture.directory);
    for owner in [
        json!({"pid":0}),
        json!({"pid":0,"managed_session_id":"session-other"}),
        json!(42),
    ] {
        write_json_atomic(&fixture.directory.join("native-session.json"), &owner).unwrap();
        let value = entry(read(&reader), "session-fault");
        assert!(value["owner"]["process_alive"].is_null());
        assert!(value["owner"]["identity_matches"].is_null());
        let flags = value["attention"].as_array().unwrap();
        assert!(flags.contains(&json!("owner_unverified")));
        assert!(!flags.contains(&json!("owner_exited")));
        assert_eq!(
            flags.contains(&json!("records_partially_unreadable")),
            owner == json!(42)
        );
    }
}

#[test]
fn owner_observation_runs_after_releasing_the_lifecycle_lock() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let observed = read(&reader);
    let writer = File::open(fixture.directory.join("turn.claim.lock")).unwrap();
    writer.try_lock().unwrap();
    let value = entry(observed, "session-fault");
    assert_eq!(value["state"], "working");
}

#[test]
fn an_older_active_result_uses_one_additional_event_decode() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    write_json_atomic(
        &fixture.directory.join("events/event-zz.json"),
        &json!({"provider":"codex","message":"later legacy result","error":null,
            "provider_session_id":null,"turn_id":null,"created_unix_ms":2}),
    )
    .unwrap();
    let (observed, reads) = with_event_read_count(|| read(&reader));
    assert_eq!(reads, 2);
    assert!(!flag(&observed, "records_partially_unreadable"));
    let expected = observed
        .observation
        .records
        .result(
            &reader,
            &Selector::Request(
                observed
                    .observation
                    .active_request()
                    .unwrap()
                    .request_id
                    .clone(),
            ),
        )
        .unwrap();
    let value = entry(observed, "session-fault");
    assert!(value["active_request"]["request_id"].is_string());
    assert_eq!(value["active_request"]["request_state"], "completed");
    assert_eq!(
        value["active_request"]["bridge_observed_elapsed_ms"],
        expected["bridge_observed_elapsed_ms"]
    );
    assert_eq!(
        value["active_request"]["bridge_observed_elapsed_reason"],
        expected["bridge_observed_elapsed_reason"]
    );
    assert_eq!(value["latest_result"]["event_id"], "event-zz.json");
    assert!(
        value["result_command"]
            .as_str()
            .unwrap()
            .contains("--request ")
    );
    fs::write(fixture.directory.join("events/event-zz.json"), b"{").unwrap();
    let (observed, reads) = with_event_read_count(|| read(&reader));
    assert_eq!(reads, 2);
    assert!(flag(&observed, "records_partially_unreadable"));
    let value = entry(observed, "session-fault");
    assert_eq!(value["active_request"]["request_state"], "completed");
    assert!(value["latest_result"].is_null());
}
