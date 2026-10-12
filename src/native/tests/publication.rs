use super::support::*;
use super::*;

#[test]
fn correlated_wait_ignores_other_completed_turns() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    for (turn_id, message) in [
        ("claude-turn-other", "other result"),
        ("claude-turn-expected", "expected result"),
    ] {
        write_event(
            directory.path(),
            &SessionEvent {
                cancelled: false,
                provider: "claude".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: Some("claude-session".to_owned()),
                turn_id: Some(turn_id.to_owned()),
                created_unix_ms: Some(unix_ms()),
            },
        )
        .unwrap();
    }
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    drop(claim);

    let event = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        Some("claude-turn-expected"),
        Some(&claim_token),
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::from_secs(1))
            .unwrap(),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(event.message, "expected result");
}

#[test]
fn completed_event_is_not_published_until_the_session_is_ready() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: std::process::id(),
            windows_process_identity: test_windows_process_identity(std::process::id()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    write_event(
        directory.path(),
        &SessionEvent {
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: Some("codex-session".to_owned()),
            turn_id: Some("codex-turn".to_owned()),
            created_unix_ms: Some(unix_ms()),
        },
    )
    .unwrap();

    let error = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        None,
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO).unwrap(),
        Duration::ZERO,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("timed out"));
}

#[test]
fn completed_event_uses_its_own_released_claim_not_a_later_turns_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let completed_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let completed_token = completed_claim.token().to_owned();
    write_event(
        directory.path(),
        &SessionEvent {
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    drop(completed_claim);
    let later_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    later_claim.retain();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    let event = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        Some(&completed_token),
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::from_secs(1))
            .unwrap(),
        Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(event.message, "completed result");
}

#[test]
fn completed_event_waits_for_its_own_claim_to_be_released() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    write_event(
        directory.path(),
        &SessionEvent {
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
        },
    )
    .unwrap();

    let error = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        Some(&claim_token),
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::ZERO).unwrap(),
        Duration::ZERO,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("timed out"));
}

#[test]
fn uncorrelated_wait_returns_the_first_event_after_its_baseline() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    for message in ["expected turn", "later turn"] {
        write_event(
            directory.path(),
            &SessionEvent {
                cancelled: false,
                provider: "codex".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: Some(unix_ms()),
            },
        )
        .unwrap();
    }

    let event = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        None,
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::from_secs(1))
            .unwrap(),
        Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(event.message, "expected turn");
}

#[test]
fn event_commit_does_not_create_a_racy_latest_cache() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    let event = SessionEvent {
        cancelled: false,
        provider: "codex".to_owned(),
        message: "committed".to_owned(),
        error: None,
        provider_session_id: None,
        turn_id: None,
        created_unix_ms: Some(1),
    };

    write_event(directory.path(), &event).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let stored: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(stored.message, "committed");
    assert!(!directory.path().join("latest.json").exists());
}

#[test]
fn provider_completion_converges_after_a_fault_before_every_mutation() {
    // Each iteration lets the completion path perform exactly `budget` filesystem
    // mutations (journal creation included) and refuses the next one, then proves that
    // the next lifecycle-lock holder converges the session from that state.
    let mut faulted = 0;
    let mut unpublished = 0;
    let mut published = 0;
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fault");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        let claim_token = claim.token().to_owned();
        claim.retain();
        let complete = || {
            turn::Report::for_claim(
                &Store::open_unchecked(&directory),
                FirstPartyCli::Codex,
                Some(&claim_token),
            )
            .complete(
                "committed result",
                Some("provider-session".to_owned()),
                Some("provider-turn".to_owned()),
            )
        };

        match with_fault_budget(budget, complete) {
            Ok(()) => break,
            Err(error) => assert!(injected_fault(&error), "{error:#}"),
        }
        faulted += 1;

        let recovered = recover_pending_completion(&directory).unwrap();
        assert_journal_settled(&directory);
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        let events = event_paths(&directory).unwrap();
        if status.state.as_str() == "working" {
            // The journal never reached its final path (a fault before its creation, its
            // content, or its rename), so nothing was published and the turn is still
            // owned by its claim; recovery had nothing to do.
            assert!(!recovered);
            assert!(directory.join(TURN_CLAIM_FILE).exists());
            assert!(events.is_empty());
            unpublished += 1;
            complete().unwrap();
        } else {
            published += 1;
        }
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        assert_journal_settled(&directory);
        let events = event_paths(&directory).unwrap();
        assert_eq!(events.len(), 1);
        let stored: SessionEvent = read_json(&events[0]).unwrap();
        assert_eq!(stored.message, "committed result");
        assert!(!recover_pending_completion(&directory).unwrap());
    }
    assert!(
        faulted >= 16,
        "only {faulted} mutation boundaries were exercised"
    );
    assert_eq!(
        unpublished, 3,
        "journal creation, content, and rename boundaries were not all exercised"
    );
    assert!(
        published >= 13,
        "post-journal boundaries were not exercised"
    );
}

#[test]
fn interrupted_close_publishes_a_journaled_completion_whose_event_was_written() {
    // Completion wrote journal and event, then the close wrote its tombstone and stopped.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-review2");
    let (request_id, event_path) = seed_event_written_completion(&directory, "late result");
    update_status(
        &directory,
        SessionState::Closed,
        None,
        Some("closed".to_owned()),
    )
    .unwrap();
    let tombstone: SessionStatus = read_json(&directory.join(CLOSED_STATUS_FILE)).unwrap();

    let before = request_state(&directory, &request_id);
    assert!(recover_pending_completion(&directory).unwrap());
    let after = request_state(&directory, &request_id);

    assert_eq!(before, ("completed".to_owned(), "closed".to_owned()));
    assert_eq!(after, before);
    assert!(event_path.exists());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert_eq!(status.generation, tombstone.generation);
    assert!(!recover_pending_completion(&directory).unwrap());
}

#[test]
fn close_cleanup_publishes_a_journaled_completion_whose_event_was_written() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-review2");
    let (request_id, event_path) = seed_event_written_completion(&directory, "late result");

    let before = request_state(&directory, &request_id);
    close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    let after = request_state(&directory, &request_id);

    assert_eq!(before.0, "completed");
    assert_eq!(after, ("completed".to_owned(), "closed".to_owned()));
    assert!(event_path.exists());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
}

#[test]
fn close_keeps_but_never_publishes_an_event_that_disagrees_with_its_journal() {
    for interrupted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-review2");
        let (request_id, event_path) =
            seed_event_written_completion(&directory, "different result");
        if interrupted {
            update_status(&directory, SessionState::Closed, None, None).unwrap();
        }

        let before = request_state(&directory, &request_id);
        if interrupted {
            assert!(recover_pending_completion(&directory).unwrap());
        } else {
            close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
        }
        let after = request_state(&directory, &request_id);

        assert_eq!(before.0, "recovery_required", "interrupted={interrupted}");
        assert_eq!(
            after,
            ("unresolved".to_owned(), "closed".to_owned()),
            "interrupted={interrupted}"
        );
        assert!(!event_path.exists());
        let quarantined = directory.join("events").join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            event_path.file_name().unwrap().to_str().unwrap()
        ));
        let kept: SessionEvent = read_json(&quarantined).unwrap();
        assert_eq!(kept.message, "different result");
        assert!(event_paths(&directory).unwrap().is_empty());
        assert!(!directory.join(TURN_COMPLETION_FILE).exists());
        assert!(!recover_pending_completion(&directory).unwrap());
    }
}

/// `--context-result <address>` resolution as `ask` and `tell` perform it: the prompt the
/// target would receive, or the resolution error.
fn cli_context_result(root: &Path, address: &str) -> Result<String> {
    let mut references = Vec::new();
    context::push_option(&mut references, address)?;
    Ok(context::resolve_in(root, &references)?.prompt_with_attachments("continue"))
}

#[test]
fn every_close_boundary_reports_a_committed_result_before_and_after_sessions() {
    for journaled_event in [
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        close_boundary_queries_agree_with(journaled_event);
    }
}

/// Stops the close before every filesystem mutation and, without recovering first, runs
/// `result`, `result --wait`, `search`, and a `--context-result` resolution over the
/// interrupted records. A committed event is reported as `completed` at every boundary,
/// before and after `sessions` converges the close; a mismatched event never is.
fn close_boundary_queries_agree_with(journaled_event: JournaledEventState) {
    let committed = journaled_event == JournaledEventState::Committed;
    let mut faulted = 0;
    let mut claim_released_before_journal = 0;
    for budget in 0.. {
        let fixture = seed_close_fixture(journaled_event);
        let root = fixture.root.path();
        let outcome = with_fault_budget(budget, || {
            close_session_state_with_error(
                &fixture.directory,
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed),
                crate::native::session::close::budget::close_deadline(),
            )
        });
        match outcome {
            Ok(()) => break,
            Err(error) => assert!(injected_fault(&error), "{error:#}"),
        }
        faulted += 1;
        let label = format!("{journaled_event:?} budget {budget}");

        // The claim is released before the journal is removed, so no boundary leaves a
        // claim that hides a committed event without the journal that proves it.
        let claim_present = fixture.directory.join(TURN_CLAIM_FILE).exists();
        let journal_present = fixture.directory.join(TURN_COMPLETION_FILE).exists();
        assert!(
            journal_present || !claim_present,
            "{label}: the claim outlived the completion journal"
        );
        if journal_present && !claim_present {
            claim_released_before_journal += 1;
        }
        let tombstoned = fixture.directory.join(CLOSED_STATUS_FILE).exists();

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} before"));

        let listing = cli_sessions(root);
        assert_eq!(listing.len(), 1, "{label}");
        assert_eq!(listing[0]["id"], "session-fault");
        // Without a tombstone the close never committed: `sessions` recovers the journal
        // as an ordinary completion instead, which publishes a committed event and
        // refuses a mismatched one.
        let expected_session_state = match (tombstoned, committed) {
            (true, _) => "closed",
            (false, true) => "ready",
            (false, false) => "working",
        };
        assert_eq!(listing[0]["state"], expected_session_state, "{label}");

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} after"));
        let after = cli_result(
            root,
            &[
                "result",
                "session-fault",
                "--request",
                &fixture.request_id,
                "--json",
            ],
        );
        assert_eq!(after["session_state"], expected_session_state, "{label}");
        assert_eq!(
            after["recovery_required"],
            !tombstoned && !committed,
            "{label}"
        );
        if tombstoned {
            assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists(), "{label}");
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label}"
            );
        }
    }
    assert!(
        faulted >= 20,
        "only {faulted} close boundaries were exercised for {journaled_event:?}"
    );
    assert!(
        claim_released_before_journal >= 2,
        "the boundaries between claim release and journal removal were not exercised for {journaled_event:?}"
    );
}

fn assert_close_boundary_queries(
    root: &Path,
    fixture: &CloseFixture,
    committed: bool,
    label: &str,
) {
    let id = "session-fault";
    let request_id = fixture.request_id.as_str();
    let plain = cli_result(root, &["result", id, "--request", request_id, "--json"]);
    let waited = cli_result(
        root,
        &[
            "result",
            id,
            "--request",
            request_id,
            "--wait",
            "--timeout-secs",
            "1",
            "--json",
        ],
    );
    let search = cli_search(
        root,
        &["search", "late result", "--all-workspaces", "--json"],
    );
    let context = cli_context_result(root, &format!("{id}/{request_id}"));
    if committed {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert_eq!(value["ok"], true, "{label}: {name}: {value}");
            assert_eq!(
                value["request_state"], "completed",
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], "late result", "{label}: {name}");
            assert_eq!(
                value["event_id"], fixture.pending.event_file,
                "{label}: {name}"
            );
            assert_eq!(
                value["timed_out"],
                serde_json::Value::Null,
                "{label}: {name}"
            );
        }
        let hits = search["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{label}: {search}");
        assert_eq!(hits[0]["session"], id, "{label}");
        assert_eq!(hits[0]["request_id"], request_id, "{label}");
        assert_eq!(hits[0]["event_id"], fixture.pending.event_file, "{label}");
        assert_eq!(search["incomplete"], false, "{label}: {search}");
        assert_eq!(search["scanned"]["events"], 1, "{label}: {search}");
        let prompt = context.unwrap_or_else(|error| panic!("{label}: {error:#}"));
        assert!(prompt.contains("late result"), "{label}: {prompt}");
    } else {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert!(
                matches!(
                    value["request_state"].as_str(),
                    Some("recovery_required" | "unresolved")
                ),
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], serde_json::Value::Null, "{label}: {name}");
        }
        assert!(
            search["hits"].as_array().unwrap().is_empty(),
            "{label}: {search}"
        );
        // While the journal and the differing event are both still in place, the scan
        // read the event to decide publication, skipped it, and says so.
        let unverified_in_place =
            fixture.directory.join(TURN_COMPLETION_FILE).exists() && fixture.event_path.exists();
        let reasons = search["incomplete_reasons"].to_string();
        assert_eq!(
            reasons.contains("differs from its pending completion journal"),
            unverified_in_place,
            "{label}: {reasons}"
        );
        assert_eq!(
            search["incomplete"], unverified_in_place,
            "{label}: {search}"
        );
        assert_eq!(
            search["scanned"]["events"],
            u64::from(unverified_in_place),
            "{label}: {search}"
        );
        let error = context.unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot be attached"),
            "{label}: {error:#}"
        );
    }
}

#[test]
fn search_charges_publication_reads_against_its_byte_budget() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-budget");
    let message = format!("needle {}", "x".repeat(4200));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    assert!(size > 4200);
    let search = |max_bytes: Option<u64>| {
        let mut args = vec!["search", "needle", "--all-workspaces", "--json"];
        let limit = max_bytes.map(|value| value.to_string());
        if let Some(limit) = &limit {
            args.extend(["--max-bytes", limit]);
        }
        cli_search(root.path(), &args)
    };

    // A one-byte budget cannot verify the journaled event's publication. The scan does
    // not read the record outside its budget: it stops and names the record instead.
    let starved = search(Some(1));
    assert!(starved["hits"].as_array().unwrap().is_empty(), "{starved}");
    assert_eq!(starved["incomplete"], true, "{starved}");
    assert_eq!(starved["scanned"]["events"], 0, "{starved}");
    let reasons = starved["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "byte budget of 1 bytes exhausted; session-budget/{event_id} is {size} bytes with 1 bytes remaining"
        )),
        "{reasons}"
    );
    let short = search(Some(size - 1));
    assert_eq!(short["incomplete"], true, "{short}");
    assert!(
        short["incomplete_reasons"]
            .to_string()
            .contains(&format!("with {} bytes remaining", size - 1)),
        "{short}"
    );

    // Exactly the event's size: the publication read consumes the whole budget and the
    // committed bytes it holds are searched without a second read.
    let exact = search(Some(size));
    let hits = exact["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{exact}");
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(exact["incomplete"], false, "{exact}");
    assert_eq!(exact["scanned"]["events"], 1, "{exact}");

    // A second session sorts after the first and holds an ordinary published hit.
    let other = root.path().join("session-other");
    fs::create_dir_all(other.join("events")).unwrap();
    write_test_manifest(&other);
    update_status(&other, SessionState::Ready, None, None).unwrap();
    let mut plain = read_json::<SessionEvent>(&event_path).unwrap();
    plain.message = "needle in the other session".to_owned();
    write_json_atomic(&other.join("events").join("event-1.json"), &plain).unwrap();
    // A mismatched journaled event is read to decide publication, then skipped and
    // reported, and its bytes still count: the same budget no longer reaches the other
    // session.
    let mut differing = read_json::<SessionEvent>(&event_path).unwrap();
    differing.message = format!("needle {}", "y".repeat(4200));
    write_json_atomic(&event_path, &differing).unwrap();
    let mismatched = search(None);
    let hits = mismatched["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{mismatched}");
    assert_eq!(hits[0]["session"], "session-other");
    assert_eq!(mismatched["incomplete"], true, "{mismatched}");
    assert_eq!(mismatched["scanned"]["events"], 2, "{mismatched}");
    let reasons = mismatched["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "{event_id}: skipped; the event differs from its pending completion journal and is not published"
        )),
        "{reasons}"
    );
    let charged = search(Some(size));
    assert!(charged["hits"].as_array().unwrap().is_empty(), "{charged}");
    assert_eq!(charged["scanned"]["events"], 1, "{charged}");
    assert_eq!(charged["scanned"]["sessions"], 1, "{charged}");
    assert!(
        charged["incomplete_reasons"].to_string().contains(&format!(
            "scan stopped: byte budget of {size} bytes exhausted"
        )),
        "{charged}"
    );
}

#[test]
fn a_journaled_event_over_the_read_limit_is_never_published_or_compared() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-limit");
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, "late result", "late result");
    let size = fs::metadata(&event_path).unwrap().len();
    let pending: PendingTurnCompletion = read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
    let read = journaled_event_state_within(&directory, &pending, size).unwrap();
    assert_eq!(read.state, JournaledEventState::Committed);
    assert_eq!(read.bytes_read, size);
    assert_eq!(
        read.committed_text.as_deref(),
        Some(fs::read_to_string(&event_path).unwrap().as_str())
    );
    let read = journaled_event_state_within(&directory, &pending, size - 1).unwrap();
    assert_eq!(read.state, JournaledEventState::Oversized(size));
    assert_eq!(read.bytes_read, 0);
    assert!(read.committed_text.is_none());
    // Beyond the limit the record is not published.
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let snapshot = Reader::open_unchecked(&directory)
        .snapshot_within(size - 1)
        .unwrap();
    let value = snapshot
        .result(
            &Reader::open_unchecked(&directory),
            &query::Selector::Request(request_id.clone()),
        )
        .unwrap();
    assert_eq!(value["request_state"], "recovery_required");
}

/// Everything a query or a lifecycle step can observe of a settled completion.
#[derive(Debug, PartialEq)]
struct SettledCompletion {
    request: (String, String),
    status: (String, Option<String>),
    event: SessionEvent,
    claim_present: bool,
    journal_present: bool,
}

fn settled_completion(directory: &Path, request_id: &str, event_path: &Path) -> SettledCompletion {
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    SettledCompletion {
        request: request_state(directory, request_id),
        status: (status.state.to_string(), status.error),
        event: read_json(event_path).unwrap(),
        claim_present: directory.join(TURN_CLAIM_FILE).exists(),
        journal_present: directory.join(TURN_COMPLETION_FILE).exists(),
    }
}

#[test]
fn a_completion_over_the_read_limit_is_journaled_as_an_explicit_failure() {
    // The real limit: a result the publication predicate could never compare is recorded
    // as a failure that names the size, and the record it publishes stays readable.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-limit");
    let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
    let event = oversized_sample_event(&"x".repeat(EVENT_READ_LIMIT as usize));
    {
        Report::for_claim(
            &Store::open_unchecked(&directory),
            FirstPartyCli::Codex,
            Some(&claim_token),
        )
        .complete(&event.message, event.provider_session_id, event.turn_id)
        .unwrap();
    }
    let settled = settled_completion(&directory, &request_id, &event_path);
    assert_eq!(settled.request, ("failed".to_owned(), "failed".to_owned()));
    let error = settled.status.1.clone().unwrap();
    assert!(
        error.contains(&format!("exceeds the {EVENT_READ_LIMIT} byte event limit")),
        "{error}"
    );
    assert_eq!(settled.event.error.as_deref(), Some(error.as_str()));
    assert_eq!(settled.event.message, "");
    assert!(fs::metadata(&event_path).unwrap().len() < EVENT_READ_LIMIT);
    assert!(!settled.claim_present);
    assert!(!settled.journal_present);
    let value = query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap();
    assert_eq!(value["error"], error);
}

#[test]
fn a_journal_over_the_read_limit_is_never_published_whichever_write_it_stopped_after() {
    // A journal written before the size policy existed carries an event over the limit.
    // Recovery refuses to publish it whether the event file is absent or present, so the
    // interruption order cannot decide the outcome; a close then settles both the same way.
    let event = oversized_sample_event(&"x".repeat(EVENT_READ_LIMIT as usize));
    let event_bytes = serde_json::to_vec_pretty(&event).unwrap();
    let size = event_bytes.len() as u64;
    assert!(size > EVENT_READ_LIMIT);
    let mut outcomes = Vec::new();
    for event_written in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-legacy");
        let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
        let mut pending = PendingTurnCompletion::new(&claim_token, event.clone(), None).unwrap();
        pending.event_file = event_path.file_name().unwrap().to_str().unwrap().to_owned();
        write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
        if event_written {
            fs::write(&event_path, &event_bytes).unwrap();
        }
        let label = format!("event_written={event_written}");

        let error = recover_pending_completion(&directory).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "{size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
            )),
            "{label}: {text}"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "working", "{label}");
        assert!(directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert_eq!(event_path.exists(), event_written, "{label}");
        let before = request_state(&directory, &request_id);
        assert_eq!(before.0, "recovery_required", "{label}");

        close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
        assert!(!event_path.exists(), "{label}");
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert!(event_paths(&directory).unwrap().is_empty(), "{label}");
        outcomes.push((before, request_state(&directory, &request_id)));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(
        outcomes[0].1,
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn close_settles_a_journaled_completion_when_the_events_directory_is_missing() {
    // A journal with no `events` directory has no event to verify: the close keeps its
    // tombstone, releases the claim, and discards the journal, and the session can then
    // be listed, closed again, and pruned. Before this fix the directory check failed the
    // close after the tombstone, leaving claim and journal installed forever.
    for journaled_event in [JournaledEventState::Committed, JournaledEventState::Absent] {
        let fixture = seed_close_fixture(journaled_event);
        let label = format!("{journaled_event:?}");
        fs::remove_dir_all(fixture.directory.join("events")).unwrap();
        let close = || {
            close_session_state_with_error(
                &fixture.directory,
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed),
                crate::native::session::close::budget::close_deadline(),
            )
        };
        let assert_settled = |phase: &str| {
            assert!(
                fixture.directory.join(CLOSED_STATUS_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_CLAIM_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join("events").exists(),
                "{label} {phase}: the close created an events directory"
            );
            assert_eq!(
                request_state(&fixture.directory, &fixture.request_id),
                ("unresolved".to_owned(), "closed".to_owned()),
                "{label} {phase}"
            );
        };

        close().unwrap();
        assert_settled("after the close");
        let tombstone_before = fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap();
        let listing = cli_sessions(fixture.root.path());
        assert_eq!(listing.len(), 1, "{label}: {listing:?}");
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        assert_settled("after sessions");
        close().unwrap();
        assert_settled("after the second close");
        assert_eq!(
            fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap(),
            tombstone_before,
            "{label}: the tombstone changed"
        );
        assert_eq!(
            prune_closed_sessions(fixture.root.path(), u128::MAX).unwrap(),
            ["session-fault"],
            "{label}"
        );
        assert!(!fixture.directory.exists(), "{label}");
    }

    // An interrupted close (tombstone written, nothing else) converges the same way.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    update_status(
        &fixture.directory,
        SessionState::Closed,
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();
    assert!(recover_pending_completion(&fixture.directory).unwrap());
    assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&fixture.directory).unwrap());
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn close_still_rejects_a_linked_events_directory() {
    // Settling a missing directory must not weaken link rejection: a link at `events`
    // fails the close, nothing behind the link is opened or moved, and the claim and
    // journal stay installed as the evidence of the unsettled completion.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("events");
    fs::rename(fixture.directory.join("events"), &target).unwrap();
    if !link_directory(&target, &fixture.directory.join("events")) {
        eprintln!("skipped: this environment cannot create a directory link");
        return;
    }
    let error = close_session_state_with_error(
        &fixture.directory,
        Some("closed by the maintainer".to_owned()),
        |_| Ok(terminal::CloseOutcome::Closed),
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(target.join(&fixture.pending.event_file).exists());
    assert!(
        !target
            .join(format!(
                "{UNPUBLISHED_EVENT_PREFIX}{}",
                fixture.pending.event_file
            ))
            .exists()
    );
    // The interrupted-close convergence refuses the link the same way.
    let error = recover_pending_completion(&fixture.directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
}

#[test]
fn claim_free_recovery_refuses_an_equivalent_event_in_another_encoding() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-compact");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    // Journal, event, terminal status, and claim release all exist, but the event bytes
    // are compact JSON: the same value, not the journal's canonical write.
    let event_path = directory.join("events").join(&pending.event_file);
    fs::write(&event_path, serde_json::to_vec(&pending.event).unwrap()).unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    let stored: SessionEvent = read_json(&event_path).unwrap();
    assert_eq!(stored, pending.event);

    let before = request_state(&directory, &request_id);
    assert_eq!(before, ("recovery_required".to_owned(), "ready".to_owned()));
    let error = recover_pending_completion(&directory).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("claim-free pending completion event does not match its journal"),
        "{error:#}"
    );
    let listing = cli_sessions(root.path());
    assert_eq!(listing[0]["state"], "ready");
    assert_eq!(listing[0]["results"], 0);
    let reader = Reader::open_unchecked(&directory);
    assert_eq!(reader.published_result_count().unwrap(), 0);
    let status = query::status::test_value_in(root.path());
    assert!(status["sessions"][0]["latest_result"].is_null());
    assert_eq!(request_state(&directory, &request_id), before);
    assert!(directory.join(TURN_COMPLETION_FILE).exists());
    assert!(event_path.exists());

    // Close settles the record the way it settles every event the journal disagrees
    // with: set aside under a name no query reads, never published.
    close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    assert_eq!(
        request_state(&directory, &request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
    assert!(!event_path.exists());
    let quarantined = directory
        .join("events")
        .join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
    let kept: SessionEvent = read_json(&quarantined).unwrap();
    assert_eq!(kept, pending.event);
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
    assert_eq!(cli_sessions(root.path())[0]["results"], 0);
}

#[test]
fn recovery_syncs_an_interrupted_quarantine_before_discarding_its_journal() {
    // A close that stops right after moving a mismatched event aside, before `events/` is
    // synced, leaves the rename unsynced while the journal still proves the event was
    // unverified. The recovery that finishes the close must sync `events/` before it
    // removes the claim and the journal, or a later crash could bring the event back
    // without the journal that keeps it unpublished.
    let mut reproduced = false;
    for budget in 0.. {
        let fixture = seed_close_fixture(JournaledEventState::Mismatched);
        let events = fixture.directory.join("events");
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &fixture.directory,
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed),
                    crate::native::session::close::budget::close_deadline(),
                )
            })
        });
        let error = match outcome {
            Ok(()) => break,
            Err(error) => error,
        };
        assert!(injected_fault(&error), "{error:#}");
        let quarantined = events.join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            fixture.pending.event_file
        ));
        // The boundary under test: the event was moved aside, and `events/` was not
        // synced afterwards. (The close reports the later cleanup steps' faults, so the
        // boundary is recognised by its state, not by the error text.)
        if !quarantined.exists() || close_log.contains(&SyncRecord::Directory(events.clone())) {
            continue;
        }
        reproduced = true;
        assert!(!fixture.event_path.exists());
        assert!(fixture.directory.join(CLOSED_STATUS_FILE).exists());
        assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());

        let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(changed.unwrap());
        let events_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(events.clone()));
        let session_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
        assert!(
            matches!(
                (events_synced, session_synced),
                (Some(events), Some(session)) if events < session
            ),
            "the quarantine rename was not made durable before the session records changed: {log:?}"
        );
        assert!(quarantined.exists());
        assert!(!fixture.event_path.exists());
        assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned())
        );
        // Converged: a second recovery changes nothing and syncs nothing.
        let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(!again.unwrap());
        assert!(log.is_empty(), "{log:?}");
        break;
    }
    assert!(
        reproduced,
        "no close boundary stopped between the quarantine rename and its sync"
    );
}

/// A completion stopped at the write boundary between the event's rename and the sync of
/// its directory entry: the event stands at its journal's path and matches it byte for
/// byte, the claim and the journal are still installed, and `events/` has not been synced
/// since the rename. Without a barrier, both recovery and close would accept the event as
/// committed and durably remove the journal, and a later crash could then lose the
/// event's directory entry together with the only evidence that it was the result.
struct UnsyncedCommittedEvent {
    _root: tempfile::TempDir,
    directory: PathBuf,
    request_id: String,
    event_path: PathBuf,
}

fn seed_completion_stopped_before_its_events_sync() -> UnsyncedCommittedEvent {
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fault");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        let claim_token = claim.token().to_owned();
        let request_id = claim.receipt().request_id.clone();
        let event_path = directory.join("events").join(&claim.receipt().event_file);
        claim.retain();
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                turn::Report::for_claim(
                    &Store::open_unchecked(&directory),
                    FirstPartyCli::Codex,
                    Some(&claim_token),
                )
                .complete(
                    "committed result",
                    Some("provider-session".to_owned()),
                    Some("provider-turn".to_owned()),
                )
            })
        });
        let error =
            outcome.expect_err("the completion finished before it stopped between rename and sync");
        assert!(injected_fault(&error), "{error:#}");
        // The boundary is recognised by its state: the event exists at its final path and
        // no sync of `events/` has happened. Every earlier budget stops before the event's
        // rename; every later one has synced the directory.
        if !event_path.exists() || log.contains(&SyncRecord::Directory(directory.join("events"))) {
            continue;
        }
        assert!(directory.join(TURN_CLAIM_FILE).exists());
        assert!(directory.join(TURN_COMPLETION_FILE).exists());
        return UnsyncedCommittedEvent {
            _root: root,
            directory,
            request_id,
            event_path,
        };
    }
    unreachable!("the fault budget sweep never ends without returning or panicking")
}

const COMMITTED_EVENT_SYNC_LABEL: &str = "syncing a committed completion event's directory";

#[test]
fn recovery_syncs_a_committed_event_before_discarding_its_journal() {
    // An uninterrupted recovery syncs `events/` before it touches any session record: the
    // status rewrite, the claim release, and the journal removal all sync the session
    // directory, and the events sync must come first.
    let fixture = seed_completion_stopped_before_its_events_sync();
    let events = fixture.directory.join("events");
    let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(changed.unwrap());
    let events_synced = log
        .iter()
        .position(|record| record == &SyncRecord::Directory(events.clone()));
    let session_synced = log
        .iter()
        .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
    assert!(
        matches!(
            (events_synced, session_synced),
            (Some(events), Some(session)) if events < session
        ),
        "the committed event was not made durable before the session records changed: {log:?}"
    );
    assert!(fixture.event_path.exists());
    assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&fixture.directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    // Converged: a repeated recovery changes nothing and syncs nothing.
    let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(!again.unwrap());
    assert!(log.is_empty(), "{log:?}");

    // Every interruption of that recovery: the claim and the journal outlive the event's
    // unsynced entry. Whenever either is gone, `events/` was synced first, and the very
    // first boundary the recovery can stop at is the sync itself.
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_completion_stopped_before_its_events_sync();
        let events = fixture.directory.join("events");
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || recover_pending_completion(&fixture.directory))
        });
        let claim_kept = fixture.directory.join(TURN_CLAIM_FILE).exists();
        let journal_kept = fixture.directory.join(TURN_COMPLETION_FILE).exists();
        if !(claim_kept && journal_kept) {
            assert!(
                log.contains(&SyncRecord::Directory(events.clone())),
                "budget {budget}: recovery discarded the journal's evidence before syncing events/: {log:?}"
            );
        }
        match outcome {
            Ok(changed) => {
                assert!(changed);
                assert!(!claim_kept && !journal_kept);
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
                assert!(fixture.event_path.exists());
                // The next holder converges (a stop after the journal's removal leaves it
                // nothing to change).
                recover_pending_completion(&fixture.directory).unwrap();
                assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
                assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "ready".to_owned())
                );
            }
        }
    }
    // Recovery reports its own first failure, so the label is exact here.
    assert!(
        labels
            .first()
            .is_some_and(|label| label.contains(COMMITTED_EVENT_SYNC_LABEL)),
        "the first recovery boundary is not the committed event's directory sync: {labels:?}"
    );
}

#[test]
fn close_syncs_a_committed_event_before_discarding_its_journal() {
    // A close of the same interrupted session writes its tombstone first, so the events
    // sync cannot precede every session-directory sync; the property is that no
    // interruption of the close, nor of the recovery that finishes an interrupted close,
    // removes the claim or the journal before `events/` was synced. The finished close
    // leaves the event published and nothing for a later recovery to sync. The close
    // reports the later cleanup steps' faults rather than the barrier's own, so the
    // boundary is recognised by its state: tombstone written, claim and journal kept,
    // `events/` not yet synced.
    let mut stopped_before_barrier = false;
    for budget in 0.. {
        let fixture = seed_completion_stopped_before_its_events_sync();
        let events = fixture.directory.join("events");
        let claim_path = fixture.directory.join(TURN_CLAIM_FILE);
        let completion_path = fixture.directory.join(TURN_COMPLETION_FILE);
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &fixture.directory,
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed),
                    crate::native::session::close::budget::close_deadline(),
                )
            })
        });
        let events_synced = close_log.contains(&SyncRecord::Directory(events.clone()));
        if !(claim_path.exists() && completion_path.exists()) {
            assert!(
                events_synced,
                "budget {budget}: close discarded the journal's evidence before syncing events/: {close_log:?}"
            );
        }
        match outcome {
            Ok(()) => {
                assert!(events_synced);
                assert!(!claim_path.exists());
                assert!(!completion_path.exists());
                assert!(fixture.event_path.exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "closed".to_owned())
                );
                let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
                assert!(!again.unwrap());
                assert!(log.is_empty(), "{log:?}");
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
            }
        }
        // Between the status rewrite and the barrier: the recovery that finishes this
        // close has no session record to rewrite before it reaches the barrier.
        let at_barrier = fixture.directory.join(CLOSED_STATUS_FILE).exists()
            && read_json::<SessionStatus>(&fixture.directory.join("status.json"))
                .is_ok_and(|status| status.state.as_str() == "closed")
            && claim_path.exists()
            && completion_path.exists()
            && !events_synced;
        stopped_before_barrier |= at_barrier;
        // The next lifecycle-lock holder finishes the interrupted close (or, before the
        // tombstone, publishes the completion); either way it must not discard the
        // journal without the barrier having run in one of the two passes.
        let (recovered, recovery_log) =
            with_sync_log(|| recover_pending_completion(&fixture.directory));
        recovered.unwrap();
        assert!(!completion_path.exists());
        assert!(!claim_path.exists());
        assert!(
            events_synced || recovery_log.contains(&SyncRecord::Directory(events.clone())),
            "budget {budget}: neither the close nor its recovery synced events/: {close_log:?} {recovery_log:?}"
        );
        if at_barrier {
            let events_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(events.clone()));
            let session_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
            assert!(
                matches!(
                    (events_synced, session_synced),
                    (Some(events), Some(session)) if events < session
                ),
                "budget {budget}: the interrupted close's recovery changed session records before syncing events/: {recovery_log:?}"
            );
        }
        assert!(fixture.event_path.exists());
        // Before the tombstone the close never committed, so recovery published the
        // completion on a session that stays ready; after it, the close is finished.
        let session_state = if fixture.directory.join(CLOSED_STATUS_FILE).exists() {
            "closed"
        } else {
            "ready"
        };
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("completed".to_owned(), session_state.to_owned()),
            "budget {budget}"
        );
    }
    assert!(
        stopped_before_barrier,
        "no close boundary stopped after the tombstone and before the committed event's directory sync"
    );
}

/// A completion whose event is committed, whose terminal status is written, and whose
/// claim is released, but whose `events/` entry was never synced: the run that released
/// the claim stopped before it removed the journal, and the sync between the event's
/// rename and the status write did not happen either (or is not trusted to have). Only
/// the journal still says the event is the result, so the claim-free recovery must sync
/// `events/` before it removes the journal, without any earlier sync helping it.
fn seed_claim_free_completion_with_an_unsynced_event() -> UnsyncedCommittedEvent {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-claimfree");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    // The journal's exact bytes at the event path, written without a sync of `events/`.
    fs::write(
        &event_path,
        serde_json::to_vec_pretty(&pending.event).unwrap(),
    )
    .unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    assert_eq!(
        journaled_event_state(&directory, &pending).unwrap(),
        JournaledEventState::Committed
    );
    UnsyncedCommittedEvent {
        _root: root,
        directory,
        request_id,
        event_path,
    }
}

#[test]
fn claim_free_recovery_syncs_the_committed_event_before_discarding_its_journal() {
    // The claim-free path has no session record to rewrite and no claim to release, so
    // its whole sync log is the barrier followed by the journal removal's own sync.
    let fixture = seed_claim_free_completion_with_an_unsynced_event();
    let events = fixture.directory.join("events");
    // Read-only queries already report the byte-identical event as published; what the
    // recovery adds is the durability of its entry before the journal goes.
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(changed.unwrap());
    assert_eq!(
        log,
        [
            SyncRecord::Directory(events.clone()),
            SyncRecord::Directory(fixture.directory.clone()),
        ],
        "{log:?}"
    );
    assert!(fixture.event_path.exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(!again.unwrap());
    assert!(log.is_empty(), "{log:?}");

    // Every interruption: the journal never goes before `events/` was synced, the first
    // boundary the path can stop at is the barrier itself, and the next holder converges
    // through the same barrier whenever the journal is still there.
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_claim_free_completion_with_an_unsynced_event();
        let events = fixture.directory.join("events");
        let completion_path = fixture.directory.join(TURN_COMPLETION_FILE);
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || recover_pending_completion(&fixture.directory))
        });
        let journal_kept = completion_path.exists();
        if !journal_kept {
            assert!(
                log.contains(&SyncRecord::Directory(events.clone())),
                "budget {budget}: the journal was discarded before events/ was synced: {log:?}"
            );
        }
        match outcome {
            Ok(changed) => {
                assert!(changed);
                assert!(!journal_kept);
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
                assert!(fixture.event_path.exists());
                let (recovered, log) =
                    with_sync_log(|| recover_pending_completion(&fixture.directory));
                let recovered = recovered.unwrap();
                assert_eq!(recovered, journal_kept, "budget {budget}");
                if journal_kept {
                    assert_eq!(
                        log[0],
                        SyncRecord::Directory(events.clone()),
                        "budget {budget}: {log:?}"
                    );
                } else {
                    assert!(log.is_empty(), "budget {budget}: {log:?}");
                }
                assert!(!completion_path.exists());
                assert!(fixture.event_path.exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "ready".to_owned())
                );
            }
        }
    }
    assert!(
        labels
            .first()
            .is_some_and(|label| label.contains(COMMITTED_EVENT_SYNC_LABEL)),
        "the first claim-free boundary is not the committed event's directory sync: {labels:?}"
    );
    // Barrier, journal removal, and the removal's sync: no other boundary exists.
    assert_eq!(labels.len(), 3, "{labels:?}");
}

#[test]
fn completion_journal_without_result_time_cannot_publish_or_release_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let mut pending = sample_completion(claim.token(), "untimed result");
    pending.event.created_unix_ms = None;
    claim.retain();
    let mut journal = serde_json::to_value(&pending).unwrap();
    journal["event"]
        .as_object_mut()
        .unwrap()
        .remove("created_unix_ms");
    assert!(serde_json::from_value::<PendingTurnCompletion>(journal.clone()).is_ok());
    write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &journal).unwrap();
    let error = recover_pending_completion(directory.path()).unwrap_err();
    assert!(format!("{error:#}").contains("missing created_unix_ms"));
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(event_paths(directory.path()).unwrap().is_empty());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}
