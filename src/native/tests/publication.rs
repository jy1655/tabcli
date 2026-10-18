use super::support::*;
use super::*;
use crate::native::query;
use crate::native::session::primitives::{event_paths, read_json, write_json_atomic};
use crate::native::session::test_support::{
    CLOSED_STATUS_FILE, SESSION_OWNER_FILE, SyncRecord, TURN_CLAIM_FILE, TURN_COMPLETION_FILE,
    injected_fault, with_fault_budget, with_sync_log, write_event,
};
use crate::native::session::turn::{
    JournaledEventState, PendingTurnCompletion, Report, journaled_event_state,
    recover_pending_completion,
};
use crate::native::session::{SessionState, SessionStatus, update_status};
use crate::native::terminal::ownership::NativeSessionOwner;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
                provenance: None,
                cancelled: false,
                provider: "claude".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: Some("claude-session".to_owned()),
                turn_id: Some(turn_id.to_owned()),
                created_unix_ms: Some(unix_ms()),
                usage: None,
                usage_error: None,
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
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: Some("codex-session".to_owned()),
            turn_id: Some("codex-turn".to_owned()),
            created_unix_ms: Some(unix_ms()),
            usage: None,
            usage_error: None,
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
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
            usage: None,
            usage_error: None,
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
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
            usage: None,
            usage_error: None,
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
                provenance: None,
                cancelled: false,
                provider: "codex".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: Some(unix_ms()),
                usage: None,
                usage_error: None,
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
        provenance: None,
        cancelled: false,
        provider: "codex".to_owned(),
        message: "committed".to_owned(),
        error: None,
        provider_session_id: None,
        turn_id: None,
        created_unix_ms: Some(1),
        usage: None,
        usage_error: None,
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
    let read = Reader::open_unchecked(&directory)
        .journaled_event_state_within(&pending, size)
        .unwrap();
    assert_eq!(read.state, JournaledEventState::Committed);
    assert_eq!(read.bytes_read, size);
    assert_eq!(
        read.committed_text.as_deref(),
        Some(fs::read_to_string(&event_path).unwrap().as_str())
    );
    let read = Reader::open_unchecked(&directory)
        .journaled_event_state_within(&pending, size - 1)
        .unwrap();
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
