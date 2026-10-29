use super::*;
use crate::native::clock::FakeClock;
use crate::native::process_identity::{Attested, Gone, ProcessObservation};
use crate::native::session::ResidualSurface;
use crate::native::session::owner::ControlAttestation::{self, *};
use crate::native::session::owner::tests::reused_attestation;
use crate::native::session::primitives::{write_json_atomic, write_private};
use crate::native::{session, tests::seed_close_fixture};
use observation::judgment::tests::{CLAIM, SESSION, attestation, evidence, receipt, snapshot};
use observation::judgment::{IndexIncomplete, Judgments, Surface, SurfaceSource, UnreadablePart};
use observation::{ActiveRequestStatus, LatestResultStatus};
use request::RequestState;

fn read(reader: &Reader) -> StatusObservation {
    Observation::read_for_status(
        reader,
        Instant::now() + SEARCH_TIME_BUDGET,
        true,
        &mut SystemClock,
    )
    .unwrap()
    .unwrap()
}

fn origin(ended: Option<bool>) -> OriginObservation {
    use crate::native::session::origin::{OriginReference, SourceKind};
    OriginObservation {
        reference: OriginReference {
            id: Some("session-origin".to_owned()),
            kind: SourceKind::Managed,
            request_id: Some("request-origin".to_owned()),
            reason: "recorded_source".to_owned(),
            detail: None,
        },
        stored_state: None,
        ended,
        reason: "fixture".to_owned(),
        detail: None,
    }
}

fn flag(observed: &StatusObservation, expected: &str) -> bool {
    attention(observed, &OwnerJudgment::Absent, &origin(None)).contains(&expected)
}

/// A StatusObservation assembled in memory from `records`, with every auxiliary record
/// readable and absent and both status reads successful.
fn in_memory(records: Snapshot) -> StatusObservation {
    let evidence = evidence();
    let judgments = Judgments::of(&records, &evidence, None, &Ok(None), None, None, SESSION);
    StatusObservation {
        observation: Observation {
            origin: session::origin::reference(&records),
            cancel: None,
            records,
            evidence,
            resumed_from: Ok(None),
            workspace_consent: consent::SessionConsentObservation::default(),
            judgments,
        },
        active: None,
        latest: Ok(None),
        questions: None,
    }
}

fn live(control: ControlAttestation) -> OwnerJudgment {
    OwnerJudgment::Bound(attestation(
        ProcessObservation::Live(Attested::Incarnation),
        control,
        Some(true),
    ))
}

#[test]
fn status_maps_each_judgment_to_its_documented_output() {
    let mut observed = in_memory(snapshot(SessionState::Working));
    // A verified owner adds no flag, so every other row is read alone.
    let flags = |observed: &StatusObservation, ended: Option<bool>| {
        attention(observed, &live(Verified), &origin(ended))
    };
    assert!(flags(&observed, None).is_empty());

    for (cancel, expected) in [
        (
            Cancel::Recorded(Some(CancelState::RequestedActive)),
            vec!["cancel_requested"],
        ),
        (Cancel::Recorded(Some(CancelState::RequestedStale)), vec![]),
        (Cancel::Recorded(Some(CancelState::Applied)), vec![]),
        (Cancel::Recorded(Some(CancelState::NotApplied)), vec![]),
        (Cancel::Recorded(None), vec![]),
        (Cancel::Unreadable("x".into()), vec![]),
        (Cancel::Absent, vec![]),
    ] {
        observed.observation.judgments.cancel = cancel.clone();
        assert_eq!(flags(&observed, None), expected, "{cancel:?}");
    }
    for (hold, expected) in [
        (
            Hold::Held {
                release_allowed: true,
            },
            vec!["held"],
        ),
        (
            Hold::Held {
                release_allowed: false,
            },
            vec!["held"],
        ),
        (Hold::Unreadable("x".into()), vec![]),
        (Hold::NotHeld, vec![]),
    ] {
        observed.observation.judgments.hold = hold.clone();
        assert_eq!(flags(&observed, None), expected, "{hold:?}");
    }
    for (turn, expected) in [
        (Turn::DeliveryUnconfirmed, vec!["delivery_unconfirmed"]),
        (
            Turn::LaunchFailed {
                reason: "launch_timeout",
                detail: "deadline expired".into(),
            },
            vec!["launch_timeout"],
        ),
        (
            Turn::LaunchFailed {
                reason: "launch_uncertain",
                detail: "spawning".into(),
            },
            vec!["launch_uncertain"],
        ),
        (Turn::TurnInProgress, vec![]),
        (Turn::NoActiveTurn, vec![]),
    ] {
        observed.observation.judgments.turn = turn.clone();
        assert_eq!(flags(&observed, None), expected, "{turn:?}");
    }
    observed.observation.judgments.claim_without_receipt = true;
    assert_eq!(flags(&observed, None), ["claim_without_receipt"]);
    observed.observation.judgments.claim_without_receipt = false;
    observed.observation.judgments.recovery_required = true;
    assert_eq!(flags(&observed, None), ["recovery_required"]);
    observed.observation.judgments.recovery_required = false;
    for part in [
        UnreadablePart::Hold,
        UnreadablePart::Owner,
        UnreadablePart::SurfaceRecord,
        UnreadablePart::UnconsumedCloseTombstone,
        UnreadablePart::ResumedFrom,
        UnreadablePart::RequestIndex,
    ] {
        observed.observation.judgments.unreadable = vec![part];
        assert_eq!(
            flags(&observed, None),
            ["records_partially_unreadable"],
            "{part:?}"
        );
    }
    observed.observation.judgments.unreadable = vec![];
    observed.observation.judgments.request_index = Some(IndexIncomplete {
        unreadable: 1,
        error: None,
    });
    assert_eq!(flags(&observed, None), ["request_index_incomplete"]);
    observed.observation.judgments.request_index = None;
    observed.observation.judgments.residual_surface = Some(ResidualSurface::Unverified);
    assert_eq!(flags(&observed, None), ["residual_surface_unverified"]);
    observed.observation.judgments.residual_surface = None;
    for (lifecycle, expected) in [
        (Lifecycle::Failed, vec!["session_failed"]),
        (Lifecycle::Exited, vec!["session_exited"]),
        (Lifecycle::Other, vec![]),
    ] {
        observed.observation.judgments.lifecycle = lifecycle;
        assert_eq!(flags(&observed, None), expected, "{lifecycle:?}");
    }
    // Status's own two reads join the unreadable parts (D12).
    observed.latest = Err(anyhow::anyhow!("latest"));
    assert_eq!(flags(&observed, None), ["records_partially_unreadable"]);
    observed.latest = Ok(None);
    observed.active = Some(Err(anyhow::anyhow!("active")));
    assert_eq!(flags(&observed, None), ["records_partially_unreadable"]);
    observed.active = Some(Ok(ActiveRequestStatus {
        state: RequestState::Pending,
        elapsed_ms: None,
        elapsed_reason: Some("no_published_result"),
    }));
    assert!(flags(&observed, None).is_empty());
    observed.active = None;
    assert_eq!(flags(&observed, Some(true)), ["origin_ended"]);
    assert!(flags(&observed, Some(false)).is_empty());

    // Owner rows: a live owner whose identity was not compared is not flagged, and an
    // Unknown attestation is unverified, never exited (B3).
    let owner_rows: Vec<(OwnerJudgment, Vec<&str>, Value)> = vec![
        (
            OwnerJudgment::Bound(attestation(
                ProcessObservation::Gone(Gone::Exited),
                NotRecorded,
                Some(false),
            )),
            vec!["owner_exited"],
            json!([false, null, null]),
        ),
        (
            OwnerJudgment::Bound(reused_attestation()),
            vec!["owner_identity_mismatch"],
            json!([true, false, null]),
        ),
        (
            live(Changed("owner TTY changed".into())),
            vec!["owner_identity_mismatch"],
            json!([true, false, null]),
        ),
        (live(Verified), vec![], json!([true, true, null])),
        (live(NotRecorded), vec![], json!([true, null, null])),
        (
            OwnerJudgment::Bound(attestation(
                ProcessObservation::Live(Attested::PidOnly),
                NotRecorded,
                Some(true),
            )),
            vec![],
            json!([true, null, null]),
        ),
        (
            live(Unknown("identity check failed".into())),
            vec!["owner_unverified"],
            json!([true, null, "identity check failed"]),
        ),
        (
            OwnerJudgment::Bound(attestation(
                ProcessObservation::Unknown("denied".into()),
                NotRecorded,
                Some(false),
            )),
            vec!["owner_unverified"],
            json!([false, null, "denied"]),
        ),
        (
            OwnerJudgment::UnboundLegacy { alive: Some(false) },
            vec!["owner_unverified"],
            json!([null, null, "owner record is unbound"]),
        ),
        (
            OwnerJudgment::Foreign,
            vec!["owner_unverified"],
            json!([null, null, FOREIGN_OWNER]),
        ),
        (
            OwnerJudgment::Absent,
            vec!["owner_unverified"],
            json!([null, null, "no owner record"]),
        ),
        (
            OwnerJudgment::Unreadable("invalid JSON".into()),
            vec!["owner_unverified"],
            json!([null, null, "invalid JSON"]),
        ),
    ];
    for (owner, expected, value) in &owner_rows {
        assert_eq!(
            &attention(&observed, owner, &origin(None)),
            expected,
            "{owner:?}"
        );
        let o = owner_value(owner);
        assert_eq!(
            &json!([o.process_alive, o.identity_matches, o.error]),
            value,
            "{owner:?}"
        );
    }
    // A closed session has neither owner nor origin attention.
    observed.observation.judgments.lifecycle = Lifecycle::Closed;
    for (owner, _, _) in &owner_rows {
        assert!(
            attention(&observed, owner, &origin(Some(true))).is_empty(),
            "{owner:?}"
        );
    }
    observed.observation.judgments.lifecycle = Lifecycle::Other;

    // Every flag at once, in the documented order.
    let judgments = &mut observed.observation.judgments;
    judgments.cancel = Cancel::Recorded(Some(CancelState::RequestedActive));
    judgments.hold = Hold::Held {
        release_allowed: false,
    };
    judgments.turn = Turn::DeliveryUnconfirmed;
    judgments.claim_without_receipt = true;
    judgments.recovery_required = true;
    judgments.unreadable = vec![UnreadablePart::RequestIndex];
    judgments.request_index = Some(IndexIncomplete {
        unreadable: 1,
        error: None,
    });
    judgments.residual_surface = Some(ResidualSurface::Unverified);
    judgments.lifecycle = Lifecycle::Failed;
    assert_eq!(
        attention(&observed, &owner_rows[0].0, &origin(Some(true))),
        [
            "cancel_requested",
            "held",
            "delivery_unconfirmed",
            "claim_without_receipt",
            "recovery_required",
            "owner_exited",
            "records_partially_unreadable",
            "request_index_incomplete",
            "residual_surface_unverified",
            "session_failed",
            "origin_ended",
        ]
    );

    for (candidate, expected) in [
        (
            ResultCandidate::ActiveRequest("request-test".into()),
            Some(format!(
                "{PUBLIC_COMMAND} result {SESSION} --request request-test --json"
            )),
        ),
        (
            ResultCandidate::ClaimOnly,
            Some(format!("{PUBLIC_COMMAND} result {SESSION} --list --json")),
        ),
        (
            ResultCandidate::LatestDecoded("event-9.json".into()),
            Some(format!(
                "{PUBLIC_COMMAND} result {SESSION} --event event-9.json --json"
            )),
        ),
        (ResultCandidate::None, None),
    ] {
        assert_eq!(
            result_command(candidate.clone(), SESSION),
            expected,
            "{candidate:?}"
        );
    }

    // The entry carries the judgments and hands status's decoded latest to the candidate.
    let mut records = snapshot(SessionState::Working);
    records.claim = Some(CLAIM.to_owned());
    records.receipts.push(receipt(CLAIM));
    let mut observed = in_memory(records);
    observed.observation.judgments.recovery_required = true;
    observed.observation.judgments.residual_surface = Some(ResidualSurface::Unverified);
    observed.observation.judgments.surface = Surface::Unreadable {
        source: SurfaceSource::ActiveHandle,
        reason: "x".into(),
    };
    observed.latest = Ok(Some(LatestResultStatus {
        event_id: Some("event-9.json".into()),
        request_id: None,
        state: RequestState::Completed,
        created_unix_ms: None,
    }));
    let value = entry(observed, SESSION, origin(None));
    assert_eq!(value["recovery_required"], true);
    assert_eq!(value["residual_surface"], "unverified");
    assert_eq!(value["held"], false);
    assert_eq!(
        value["result_command"],
        format!("{PUBLIC_COMMAND} result {SESSION} --request request-test --json")
    );
    assert_eq!(value["derived_from"], "observation");
    let mut observed = in_memory(snapshot(SessionState::Ready));
    observed.latest = Ok(Some(LatestResultStatus {
        event_id: Some("event-9.json".into()),
        request_id: None,
        state: RequestState::Completed,
        created_unix_ms: None,
    }));
    let value = entry(observed, SESSION, origin(None));
    assert_eq!(value["recovery_required"], false);
    assert!(value.get("residual_surface").is_none());
    assert_eq!(
        value["result_command"],
        format!("{PUBLIC_COMMAND} result {SESSION} --event event-9.json --json")
    );
}

#[test]
fn status_decodes_only_latest_and_reuses_it_for_active_elapsed() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    write_private(&fixture.directory.join("events/event-000.json"), b"{").unwrap();
    let (observed, reads) = with_event_read_count(|| read(&reader));
    assert_eq!(reads, 1);
    assert!(!flag(&observed, "records_partially_unreadable"));
    let value = entry(observed, "session-fault", origin(None));
    assert!(value["active_request"].get("result").is_none());
    assert!(value["latest_result"].get("result").is_none());
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
    let value = entry(read(&reader), "session-fault", origin(None));
    assert_eq!(value["active_request"]["request_state"], "pending");
    assert!(value["active_request"]["bridge_observed_elapsed_ms"].is_null());
    assert_eq!(
        value["active_request"]["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
}

#[test]
fn an_unreadable_active_result_keeps_its_request_and_null_fields() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let (observed, reads) = with_event_read_count(|| {
        with_event_read_hook(|path| fs::write(path, b"{").unwrap(), || read(&reader))
    });
    assert_eq!(reads, 1);
    assert!(flag(&observed, "records_partially_unreadable"));
    let request_id = observed
        .observation
        .active_request()
        .unwrap()
        .request_id
        .clone();
    let value = entry(observed, "session-fault", origin(None));
    assert_eq!(
        value["active_request"],
        json!({"request_id": request_id,
            "request_state": null, "bridge_observed_elapsed_ms": null,
            "bridge_observed_elapsed_reason": "unreadable_result"})
    );
    assert!(value["latest_result"].is_null());
    assert_eq!(
        value["result_command"],
        format!("{PUBLIC_COMMAND} result session-fault --request {request_id} --json")
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
    for (retry_window, budget, scanned) in [
        (Duration::from_secs(5), Duration::from_secs(2), 1),
        (SNAPSHOT_RETRY_WINDOW, Duration::from_millis(400), 2),
    ] {
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let value = with_snapshot_retry_window(retry_window, || {
            value_in_with_clock(root.path(), &all(), budget, &mut clock)
        })
        .unwrap();
        assert_eq!(clock.slept, budget);
        assert_eq!(clock.now(), start + budget);
        assert_eq!(value["scanned"], json!({"sessions":scanned,"listed":0}));
        let reasons = value["incomplete_reasons"].as_array().unwrap();
        assert_eq!(reasons.len(), 8);
        for (index, reason) in reasons.iter().enumerate() {
            assert_eq!(reason["session"], format!("session-busy{index}"));
            let expected = if index < scanned {
                "busy"
            } else {
                "budget exhausted"
            };
            assert!(reason["reason"].as_str().unwrap().contains(expected));
        }
    }
}

#[test]
fn snapshot_retry_keeps_its_first_read_after_the_deadline() {
    use std::cell::Cell;
    use std::rc::Rc;

    for busy in [false, true] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let attempts = Rc::new(Cell::new(0));
        let observed_attempts = Rc::clone(&attempts);
        let result = with_snapshot_hook(
            move |directory| {
                observed_attempts.set(observed_attempts.get() + 1);
                if busy {
                    let path = directory.join("status.json");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes.push(b' ');
                    fs::write(path, bytes).unwrap();
                }
            },
            || {
                observe_snapshot_until(
                    &reader,
                    PublicationRead::Within(EVENT_READ_LIMIT),
                    start - Duration::from_millis(1),
                    &mut clock,
                )
            },
        );
        if busy {
            assert!(result.err().unwrap().is::<SnapshotBusy>());
        } else {
            assert!(result.is_ok());
        }
        assert_eq!(attempts.get(), 1);
        assert_eq!(clock.slept, Duration::ZERO);
    }
}

#[test]
fn status_observation_checks_deadline_before_and_after_event_decode() {
    for expire_at_snapshot in [true, false] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        // The active Request needs a second decode if the latest Result leaves time.
        write_json_atomic(
            &fixture.directory.join("events/event-zz.json"),
            &json!({"provider":"codex","message":"later legacy result","error":null,
                "provider_session_id":null,"turn_id":null,"created_unix_ms":2}),
        )
        .unwrap();
        let mut clock = FakeClock::new(Instant::now());
        let budget = Duration::from_millis(50);
        let deadline = clock.now() + budget;
        let snapshot_time = clock.shared();
        let event_time = clock.shared();
        let (value, reads) = with_event_read_count(|| {
            with_snapshot_hook(
                move |_| {
                    if expire_at_snapshot {
                        snapshot_time.set(deadline);
                    }
                },
                || {
                    with_event_read_hook(
                        move |_| {
                            if !expire_at_snapshot {
                                event_time.set(deadline);
                            }
                        },
                        || {
                            value_in_with_clock(
                                fixture.directory.parent().unwrap(),
                                &all(),
                                budget,
                                &mut clock,
                            )
                            .unwrap()
                        },
                    )
                },
            )
        });
        assert_eq!(reads, usize::from(!expire_at_snapshot));
        assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
        assert_eq!(
            value["incomplete_reasons"],
            json!([{"session":"session-fault","reason":"status time budget exhausted"}])
        );
        assert_eq!(clock.now(), deadline);
        assert_eq!(clock.slept, Duration::ZERO);
    }
}

#[test]
fn origin_consumes_the_remaining_scan_budget_without_discarding_the_session() {
    use session::origin::tests::{Checkpoint, Hooks, Session};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let root = tempfile::tempdir().unwrap();
    Session::started_from(root.path(), "session-a", "ready", "session-z");
    Session::new(root.path(), "session-b", "ready");
    Session::new(root.path(), "session-z", "ready");
    let start = Instant::now();
    let mut clock = FakeClock::new(start);
    let snapshot_time = clock.shared();
    let origin_time = clock.shared();
    let reads = Rc::new(RefCell::new(Vec::new()));
    let observed_reads = Rc::clone(&reads);
    let origin_reads = Rc::new(Cell::new(0));
    let observed_origin_reads = Rc::clone(&origin_reads);
    let hooks = Hooks::new();
    hooks.checkpoint(move |point, directory| {
        assert_eq!(point, Checkpoint::OriginRead);
        assert_eq!(directory.file_name().unwrap(), "session-z");
        observed_origin_reads.set(observed_origin_reads.get() + 1);
        assert_eq!(origin_time.get(), start + Duration::from_millis(10));
        // The Origin's own 100 ms allowance is longer than the 40 ms left in the scan.
        origin_time.set(origin_time.get() + Duration::from_millis(40));
    });
    let value = with_snapshot_hook(
        move |directory| {
            observed_reads
                .borrow_mut()
                .push(directory.file_name().unwrap().to_str().unwrap().to_owned());
            snapshot_time.set(snapshot_time.get() + Duration::from_millis(10));
        },
        || value_in_with_clock(root.path(), &all(), Duration::from_millis(50), &mut clock),
    )
    .unwrap();
    assert_eq!(*reads.borrow(), ["session-a"]);
    assert_eq!(origin_reads.get(), 1);
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":1}));
    assert_eq!(value["sessions"][0]["id"], "session-a");
    assert_eq!(value["sessions"][0]["origin"], "session-z");
    assert_eq!(
        value["sessions"][0]["origin_observation"]["reason"],
        "origin_budget_exhausted"
    );
    assert!(value["sessions"][0]["origin_observation"]["ended"].is_null());
    assert_eq!(
        value["incomplete_reasons"],
        json!([
            {"session":"session-b","reason":"status time budget exhausted"},
            {"session":"session-z","reason":"status time budget exhausted"}
        ])
    );
    assert_eq!(clock.now(), start + Duration::from_millis(50));
    assert_eq!(clock.slept, Duration::ZERO);
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
        Observation::read_for_status(
            &reader,
            Instant::now() + SEARCH_TIME_BUDGET,
            false,
            &mut SystemClock,
        )
        .unwrap()
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
fn owner_observation_runs_after_releasing_the_lifecycle_lock() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let observed = read(&reader);
    let writer = File::open(fixture.directory.join("turn.claim.lock")).unwrap();
    writer.try_lock().unwrap();
    let value = entry(observed, "session-fault", origin(None));
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
    let value = entry(observed, "session-fault", origin(None));
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
    let value = entry(observed, "session-fault", origin(None));
    assert_eq!(value["active_request"]["request_state"], "completed");
    assert!(value["latest_result"].is_null());
}

#[test]
fn enumeration_faults_keep_status_reason_identity_and_encounter_order() {
    use session::enumerate::{TestEntry, with_entries};
    let root = tempfile::tempdir().unwrap();
    let entries = vec![
        Err(std::io::Error::other("entry fault")),
        Ok(TestEntry {
            id: "invalid".into(),
            directory: root.path().into(),
            directory_kind: Err(std::io::Error::other("ignored")),
        }),
        Ok(TestEntry {
            id: "session-z".into(),
            directory: root.path().into(),
            directory_kind: Err(std::io::Error::other("kind fault")),
        }),
        Ok(TestEntry {
            id: "session-a".into(),
            directory: root.path().join("session-a"),
            directory_kind: Ok(true),
        }),
    ];
    let value = with_entries(entries, || test_value_in(root.path()));
    let reasons = value["incomplete_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 3);
    assert_eq!(
        reasons[0],
        json!({"session": null, "reason": "failed to read session entry: entry fault"})
    );
    assert_eq!(
        reasons[1],
        json!({"session": "session-z", "reason": "failed to read session entry file type: kind fault"})
    );
    assert_eq!(reasons[2]["session"], "session-a");
}

// Keep byte-level expectations while production attention is typed.
fn attention(
    observed: &StatusObservation,
    owner: &OwnerJudgment,
    origin: &OriginObservation,
) -> Vec<&'static str> {
    super::attention(observed, owner, origin)
        .into_iter()
        .map(AttentionFlag::name)
        .collect()
}
