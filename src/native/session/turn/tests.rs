use super::*;
use crate::native::query;
use crate::native::reopen::tests::snapshot_directory;
use crate::native::session::close as session_close;
use crate::native::session::names::{TURN_CLAIM_FILE, TURN_COMPLETION_FILE};
use crate::native::session::primitives::{
    injected_fault, injected_sync_failure, lock_turn_claim, read_json, with_fault_budget,
    with_sync_failure, write_json_atomic, write_private,
};
use crate::native::session::state::update_status_with_residual;
use crate::native::session::{Reader, Store};
use crate::native::session::{launch, turn};
use crate::native::terminal;
use std::fs;

fn fixture(state: SessionState) -> (tempfile::TempDir, Store) {
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir()
        .unwrap();
    let store = Store::open_unchecked(directory.path());
    fs::create_dir(store.record(CoreRecord::Events).path()).unwrap();
    update_status(store.directory(), state, None, None).unwrap();
    (directory, store)
}

fn event(message: &str) -> SessionEvent {
    SessionEvent {
        provenance: None,
        cancelled: false,
        provider: "codex".to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: Some(1),
        usage: None,
        usage_error: None,
    }
}

#[test]
fn journal_comparison_requires_exact_bytes_inside_the_read_limit() {
    let pending = PendingTurnCompletion::new("1-2-3", event("한글 result"), None).unwrap();
    let canonical = serde_json::to_vec_pretty(&pending.event).unwrap();
    let limit = canonical.len() as u64;
    assert_eq!(
        journaled_event_state_of(&pending, &canonical, limit).unwrap(),
        JournaledEventState::Committed
    );
    assert_eq!(
        journaled_event_state_of(&pending, &canonical, limit - 1).unwrap(),
        JournaledEventState::Oversized(limit)
    );
    for bytes in [
        serde_json::to_vec(&pending.event).unwrap(),
        b"\xff".to_vec(),
        Vec::new(),
    ] {
        assert_eq!(
            journaled_event_state_of(&pending, &bytes, limit).unwrap(),
            JournaledEventState::Mismatched
        );
    }
}

#[test]
fn delivery_settlement_regression_late_not_sent_preserves_newer_turn() {
    let (_directory, store) = fixture(SessionState::Working);
    let mut old = claim(&store, &[]).unwrap();
    Report::for_claim(&store, FirstPartyCli::Codex, Some(old.token()))
        .complete("already completed", None, None)
        .unwrap();
    let (newer, _) = claim_ready(&store, "session-test", &[]).unwrap();
    let before = fs::read(store.record(CoreRecord::Status).path()).unwrap();
    old.settle_delivery(Delivery::NotSent(&anyhow::anyhow!("late not sent")))
        .unwrap();
    drop(old);
    assert_eq!(
        fs::read(store.record(CoreRecord::Status).path()).unwrap(),
        before
    );
    assert_eq!(
        current_claim_token(&store).unwrap().as_deref(),
        Some(newer.token())
    );
    assert_eq!(store.events().unwrap().len(), 1);
    newer.retain();
}

#[test]
fn delivery_settlement_regression_not_sent_keeps_failure_reason() {
    let (_directory, store) = fixture(SessionState::Ready);
    let (mut claim, _) = claim_ready(&store, "session-test", &[]).unwrap();
    update_status(store.directory(), SessionState::Working, None, None).unwrap();
    let failure = terminal::TerminalSendFailure::not_sent(anyhow::anyhow!("delivery refused"));
    {
        let delivery_error = failure.error();
        let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
            turn::Delivery::Uncertain(delivery_error)
        } else {
            turn::Delivery::NotSent(delivery_error)
        });
    };
    drop(claim);
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Ready);
    assert_eq!(status.error.as_deref(), Some("delivery refused"));
    assert!(current_claim_token(&store).unwrap().is_none());
    assert!(store.events().unwrap().is_empty());
}

#[test]
fn report_complete_publishes_once_and_releases_exclusive_ownership() {
    let (_directory, store) = fixture(SessionState::Working);
    let claimed = claim(&store, &[]).unwrap();
    assert!(claim(&store, &[]).is_err());
    assert!(!published(&store, Some(claimed.token())).unwrap());
    let request_id = claimed.receipt().request_id.clone();
    let event_name = claimed.receipt().event_file.clone();
    Report::for_claim(&store, FirstPartyCli::Codex, Some(claimed.token()))
        .complete("completed", None, None)
        .unwrap();
    assert!(published(&store, Some(claimed.token())).unwrap());
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert_eq!(store.events().unwrap().len(), 1);
    let completed = store.event_strict(&event_name).unwrap();
    assert_eq!(completed.message, "completed");
    assert!(completed.error.is_none());
    assert!(completed.created_unix_ms.is_some());
    assert_eq!(
        requests::list(&store).unwrap().receipts[0].request_id,
        request_id
    );
    // Repeating the provider completion cannot publish twice.
    Report::for_claim(&store, FirstPartyCli::Codex, Some(claimed.token()))
        .complete("duplicate", None, None)
        .unwrap();
    Report::for_claim(&store, FirstPartyCli::Codex, Some(claimed.token()))
        .complete("duplicate", None, None)
        .unwrap();
    assert_eq!(store.events().unwrap().len(), 1);
    assert!(claim(&store, &[]).is_ok());
}

#[test]
fn delivery_outcomes_keep_claims_and_receipts_distinct_from_results() {
    for initial in [false, true] {
        for outcome in ["sent", "not_sent", "uncertain"] {
            let (_directory, store) = fixture(if initial {
                SessionState::AwaitingInitialInput
            } else {
                SessionState::Ready
            });
            let mut claimed = if initial {
                claim(&store, &[]).unwrap()
            } else {
                claim_ready(&store, "session-test", &[]).unwrap().0
            };
            let request = claimed.receipt().request_id.clone();
            claimed.begin_delivery().unwrap();
            let error = anyhow::anyhow!("delivery evidence");
            claimed
                .settle_delivery(match outcome {
                    "sent" => Delivery::Sent,
                    "not_sent" => Delivery::NotSent(&error),
                    _ => Delivery::Uncertain(&error),
                })
                .unwrap();
            drop(claimed);
            let status = store.status().unwrap();
            assert_eq!(
                status.state,
                if outcome == "not_sent" {
                    if initial {
                        SessionState::Failed
                    } else {
                        SessionState::Ready
                    }
                } else {
                    SessionState::Working
                }
            );
            assert_eq!(
                status.error.as_deref(),
                if outcome == "sent" {
                    None
                } else {
                    Some("delivery evidence")
                }
            );
            assert_eq!(
                current_claim_token(&store).unwrap().is_some(),
                outcome != "not_sent"
            );
            assert_eq!(
                requests::list(&store).unwrap().receipts[0].request_id,
                request
            );
            assert!(store.events().unwrap().is_empty());
        }
    }
}

#[test]
fn delivery_reports_after_completion_never_resurrect_or_mutate_a_turn() {
    for successor in [false, true] {
        for outcome in ["sent", "not_sent", "uncertain"] {
            let (_directory, store) = fixture(SessionState::Working);
            let mut old = claim(&store, &[]).unwrap();
            Report::for_claim(&store, FirstPartyCli::Codex, Some(old.token()))
                .complete("completed before the sender settled", None, None)
                .unwrap();
            let newer = successor.then(|| claim_ready(&store, "session-test", &[]).unwrap().0);
            let before = fs::read(store.record(CoreRecord::Status).path()).unwrap();
            let token = current_claim_token(&store).unwrap();
            let error = anyhow::anyhow!("late delivery report");
            old.settle_delivery(match outcome {
                "sent" => Delivery::Sent,
                "not_sent" => Delivery::NotSent(&error),
                _ => Delivery::Uncertain(&error),
            })
            .unwrap();
            drop(old);
            assert_eq!(
                fs::read(store.record(CoreRecord::Status).path()).unwrap(),
                before
            );
            assert_eq!(current_claim_token(&store).unwrap(), token);
            assert_eq!(store.events().unwrap().len(), 1);
            if let Some(newer) = newer {
                newer.retain();
            }
        }
    }
}

#[test]
fn delivery_settlement_after_close_preserves_the_tombstone() {
    for interrupted in [false, true] {
        for outcome in ["sent", "not_sent", "uncertain"] {
            let (_directory, store) = fixture(SessionState::Working);
            let mut claimed = claim(&store, &[]).unwrap();
            claimed.begin_delivery().unwrap();
            update_status_with_residual(
                store.directory(),
                SessionState::Working,
                None,
                Some("unverified surface".to_owned()),
                Some(launch::ResidualSurface::Unverified),
            )
            .unwrap();
            if interrupted {
                // A close committed its tombstone before claim/journal cleanup.
                let pending =
                    PendingTurnCompletion::new(claimed.token(), event("unpublished"), None)
                        .unwrap();
                store.write_completion(&pending).unwrap();
                let mut closed = store.status().unwrap();
                closed.state = SessionState::Closed;
                closed.generation += 1;
                store.write_closed(&closed).unwrap();
            } else {
                session::close::close(
                    &store,
                    None,
                    crate::native::session::close::budget::close_deadline(),
                    |_| panic!("no bound surface"),
                )
                .unwrap();
            }
            let tombstone = store.record(CoreRecord::Closed).bytes().unwrap().unwrap();
            let error = anyhow::anyhow!("late delivery report");
            claimed
                .settle_delivery(match outcome {
                    "sent" => Delivery::Sent,
                    "not_sent" => Delivery::NotSent(&error),
                    _ => Delivery::Uncertain(&error),
                })
                .unwrap();
            drop(claimed);
            assert_eq!(
                store.record(CoreRecord::Closed).bytes().unwrap().unwrap(),
                tombstone
            );
            assert!(store.events().unwrap().is_empty());
            // Sent only retains; Uncertain may restore status. The normal convergence
            // owns any still-pending close cleanup, not delivery settlement.
            store.converge().unwrap();
            assert_eq!(
                store.record(CoreRecord::Status).bytes().unwrap().unwrap(),
                tombstone
            );
            assert!(current_claim_token(&store).unwrap().is_none());
            assert!(!store.record(CoreRecord::Completion).path().exists());
            assert!(store.events().unwrap().is_empty());
        }
    }
}

#[test]
fn delivery_refusal_recovers_a_completion_before_rolling_back() {
    let (_directory, store) = fixture(SessionState::Working);
    let mut claimed = claim(&store, &[]).unwrap();
    let pending = PendingTurnCompletion::new(claimed.token(), event("completed"), None).unwrap();
    store.write_completion(&pending).unwrap();
    claimed
        .settle_delivery(Delivery::NotSent(&anyhow::anyhow!("late refusal")))
        .unwrap();
    drop(claimed);
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert!(store.status().unwrap().error.is_none());
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_eq!(
        store.event_strict(&pending.event_file).unwrap(),
        event("completed")
    );
}

#[test]
fn delivery_begin_refuses_closed_and_replaced_claims() {
    for closed in [false, true] {
        let (_directory, store) = fixture(SessionState::Working);
        let mut old = claim(&store, &[]).unwrap();
        Report::for_claim(&store, FirstPartyCli::Codex, Some(old.token()))
            .complete("completed", None, None)
            .unwrap();
        let newer = if closed {
            session::close::close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |_| panic!("no surface"),
            )
            .unwrap();
            None
        } else {
            Some(claim_ready(&store, "session-test", &[]).unwrap().0)
        };
        let before = fs::read(store.record(CoreRecord::Status).path()).unwrap();
        assert!(old.begin_delivery().is_err());
        assert_eq!(
            fs::read(store.record(CoreRecord::Status).path()).unwrap(),
            before
        );
        if let Some(newer) = newer {
            newer.retain();
        }
    }
}

#[test]
fn delivery_uncertainty_and_post_send_cleanup_failures_keep_the_claim() {
    for cleanup in [false, true] {
        let (_directory, store) = fixture(SessionState::Working);
        let mut claimed = claim(&store, &[]).unwrap();
        if cleanup {
            fs::create_dir(store.record(CoreRecord::InitialPrompt).path()).unwrap();
            assert!(claimed.complete_initial_delivery().is_err());
        } else {
            let path = store.record(CoreRecord::Status).path().to_owned();
            fs::remove_file(&path).unwrap();
            fs::create_dir(path).unwrap();
            assert!(
                claimed
                    .settle_delivery(Delivery::Uncertain(&anyhow::anyhow!("uncertain")))
                    .is_err()
            );
        }
        let token = claimed.token().to_owned();
        drop(claimed);
        assert_eq!(current_claim_token(&store).unwrap(), Some(token));
        assert!(claim(&store, &[]).is_err());
        assert!(store.events().unwrap().is_empty());
    }
}

#[test]
fn report_fail_records_failure_and_releases_ownership() {
    let (_directory, store) = fixture(SessionState::Working);
    let claimed = claim(&store, &[]).unwrap();
    let event_name = claimed.receipt().event_file.clone();
    Report::for_claim(&store, FirstPartyCli::Pi, Some(claimed.token()))
        .fail("turn failed", None, None)
        .unwrap();
    let result = store.event_strict(&event_name).unwrap();
    assert_eq!(result.provider, "pi");
    assert_eq!(result.error.as_deref(), Some("turn failed"));
    assert!(result.message.is_empty());
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some("turn failed")
    );
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert!(current_claim_token(&store).unwrap().is_none());
    assert!(published(&store, Some(claimed.token())).unwrap());
}

#[test]
fn dropped_claim_rolls_back_without_publishing() {
    let (_directory, store) = fixture(SessionState::Ready);
    let (claimed, baseline) = claim_ready(&store, "session-test", &[]).unwrap();
    assert_eq!(baseline, 0);
    assert_eq!(store.status().unwrap().state, SessionState::Claimed);
    assert!(current_claim_token(&store).unwrap().is_some());
    drop(claimed);
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert!(current_claim_token(&store).unwrap().is_none());
    assert!(store.events().unwrap().is_empty());
    // Initial claims also roll back, without imposing a ready status.
    update_status(store.directory(), SessionState::Claimed, None, None).unwrap();
    let claimed = claim(&store, &[]).unwrap();
    drop(claimed);
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_eq!(store.status().unwrap().state, SessionState::Claimed);
}

#[test]
fn recovery_publishes_once_at_every_partial_completion() {
    for completed_mutations in 0..=3 {
        let (_directory, store) = fixture(SessionState::Working);
        let claimed = claim(&store, &[]).unwrap();
        let token = claimed.token().to_owned();
        let name = claimed.receipt().event_file.clone();
        let mut pending =
            PendingTurnCompletion::new(&token, event("committed result"), None).unwrap();
        pending.event_file = name.clone();
        claimed.retain();
        store.write_completion(&pending).unwrap();
        if completed_mutations >= 1 {
            write_completion_event(store.directory(), &pending).unwrap();
        }
        if completed_mutations >= 2 {
            update_status(store.directory(), SessionState::Ready, None, None).unwrap();
        }
        if completed_mutations >= 3 {
            release_claim_token(store.record(CoreRecord::TurnClaim).path(), &token).unwrap();
        }
        assert!(recover_pending_completion(store.directory()).unwrap());
        assert!(!recover_pending_completion(store.directory()).unwrap());
        assert_eq!(store.events().unwrap().len(), 1);
        assert_eq!(
            store.event_strict(&name).unwrap(),
            event("committed result")
        );
        assert!(current_claim_token(&store).unwrap().is_none());
        assert!(
            store
                .record(CoreRecord::Completion)
                .text()
                .unwrap()
                .is_none()
        );
        assert_eq!(store.status().unwrap().state, SessionState::Ready);
        assert!(published(&store, Some(&token)).unwrap());
    }
}

#[test]
fn stale_report_cannot_complete_or_fail_a_replacement_claim() {
    let (_directory, store) = fixture(SessionState::Working);
    let stale = claim(&store, &[]).unwrap();
    let stale_token = stale.token().to_owned();
    drop(stale);
    let current = claim(&store, &[]).unwrap();
    let generation = store.status().unwrap().generation;
    let report = Report::for_claim(&store, FirstPartyCli::Claude, Some(&stale_token));
    report
        .complete("stale result", Some("claude-session".to_owned()), None)
        .unwrap();
    report.fail("stale failure", None, None).unwrap();
    assert_eq!(
        current_claim_token(&store).unwrap().as_deref(),
        Some(current.token())
    );
    assert!(store.events().unwrap().is_empty());
    assert_eq!(store.status().unwrap().generation, generation);
    assert_eq!(store.status().unwrap().state, SessionState::Working);
    Report::for_claim(&store, FirstPartyCli::Claude, Some(current.token()))
        .complete("current", None, None)
        .unwrap();
    assert_eq!(store.events().unwrap().len(), 1);
    assert!(current_claim_token(&store).unwrap().is_none());
}

#[test]
fn dropping_a_failed_completion_keeps_its_journal_recoverable() {
    let (_directory, store) = fixture(SessionState::Working);
    let mut claimed = claim(&store, &[]).unwrap();
    let token = claimed.token().to_owned();
    let name = claimed.receipt().event_file.clone();
    claimed.settle_delivery(Delivery::Sent).unwrap();
    let error = with_sync_failure(store.record(CoreRecord::Events).path(), || {
        Report::for_claim(&store, FirstPartyCli::Codex, Some(claimed.token())).complete(
            "interrupted completion",
            None,
            None,
        )
    })
    .unwrap_err();
    assert!(injected_sync_failure(&error));
    drop(claimed);
    assert_eq!(
        current_claim_token(&store).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert!(recover_pending_completion(store.directory()).unwrap());
    assert!(!recover_pending_completion(store.directory()).unwrap());
    assert_eq!(store.events().unwrap().len(), 1);
    assert_eq!(
        store.event_strict(&name).unwrap().message,
        "interrupted completion"
    );
    assert!(current_claim_token(&store).unwrap().is_none());
}

#[test]
fn retained_claim_stays_exclusive_until_report_completes() {
    let (_directory, store) = fixture(SessionState::Working);
    let claimed = claim(&store, &[]).unwrap();
    let token = claimed.token().to_owned();
    claimed.retain();
    assert!(claim(&store, &[]).is_err());
    Report::for_claim(&store, FirstPartyCli::Codex, Some(&token))
        .complete("done", None, None)
        .unwrap();
    assert!(claim(&store, &[]).is_ok());
    assert_eq!(store.events().unwrap().len(), 1);
}

fn hold_fixture(state: SessionState) -> (tempfile::TempDir, Store) {
    let (directory, store) = fixture(state);
    store
        .record(CoreRecord::Manifest)
        .write_json(&serde_json::json!({
            "schema": 1, "id": directory.path().file_name().unwrap().to_str().unwrap(),
            "provider": "codex", "provider_path": "codex", "provider_version": "0.159.3",
            "workspace": directory.path(), "title": "hold fixture", "model": null,
            "effort": null, "yolo": false, "created_unix_ms": 1
        }))
        .unwrap();
    (directory, store)
}

fn assert_hold_receipt_unresolved(store: &Store) {
    let receipts = requests::list(store).unwrap().receipts;
    assert_eq!(receipts.len(), 1);
    let value = crate::native::query::request_result(store, &receipts[0].request_id).unwrap();
    assert_eq!(value["request_state"], "unresolved");
}

// H8: each callback is outside the lifecycle lock, so the production writer can run.
#[test]
fn hold_after_first_admission_refuses_before_claim_creation() {
    let (_directory, store) = hold_fixture(SessionState::Ready);
    let result = claim_ready_with_callbacks(
        store.directory(),
        "session-test",
        || {
            session::hold::change(&store, false).unwrap();
        },
        || Ok(()),
        || {},
        &[],
    );
    assert!(result.is_err());
    assert!(current_claim_token(&store).unwrap().is_none());
    assert!(requests::list(&store).unwrap().receipts.is_empty());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
}

#[test]
fn hold_after_claim_before_claimed_keeps_unresolved_receipt() {
    let (_directory, store) = hold_fixture(SessionState::Ready);
    let result = claim_ready_with_callbacks(
        store.directory(),
        "session-test",
        || {},
        || {
            session::hold::change(&store, false)?;
            Ok(())
        },
        || {},
        &[],
    );
    assert!(result.is_err());
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_hold_receipt_unresolved(&store);
    assert!(store.events().unwrap().is_empty());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
}

#[test]
fn hold_after_claimed_before_delivery_rolls_back_with_reason() {
    let (_directory, store) = hold_fixture(SessionState::Ready);
    let (mut claimed, _) = claim_ready(&store, "session-test", &[]).unwrap();
    session::hold::change(&store, false).unwrap();
    let error = claimed.begin_delivery().unwrap_err();
    assert_eq!(
        error.to_string(),
        "session session-test is held; release the hold before sending a follow-up"
    );
    claimed.settle_delivery(Delivery::NotSent(&error)).unwrap();
    drop(claimed);
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(error.to_string().as_str())
    );
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_hold_receipt_unresolved(&store);
    assert!(store.events().unwrap().is_empty());
}

#[test]
fn hold_after_delivery_began_does_not_refuse_delivery() {
    for uncertain in [false, true] {
        let (_directory, store) = hold_fixture(SessionState::Ready);
        let (mut claimed, _) = claim_ready(&store, "session-test", &[]).unwrap();
        claimed.begin_delivery().unwrap();
        session::hold::change(&store, false).unwrap();
        let error = anyhow::anyhow!("delivery uncertain");
        claimed
            .settle_delivery(if uncertain {
                Delivery::Uncertain(&error)
            } else {
                Delivery::Sent
            })
            .unwrap();
        drop(claimed);
        assert_eq!(store.status().unwrap().state, SessionState::Working);
        assert!(current_claim_token(&store).unwrap().is_some());
        assert_eq!(requests::list(&store).unwrap().receipts.len(), 1);
        assert!(store.events().unwrap().is_empty());
    }
}

#[test]
fn initial_delivery_ignores_preexisting_valid_and_unreadable_hold() {
    for malformed in [false, true] {
        let (_directory, store) = hold_fixture(SessionState::AwaitingInitialInput);
        if malformed {
            store
                .record(CoreRecord::Hold)
                .write_private(b"malformed")
                .unwrap();
        } else {
            session::hold::change(&store, false).unwrap();
        }
        let mut initial = claim(&store, &[]).unwrap();
        initial.begin_delivery().unwrap();
        initial.settle_delivery(Delivery::Sent).unwrap();
        drop(initial);
        assert_eq!(store.status().unwrap().state, SessionState::Working);
        assert!(current_claim_token(&store).unwrap().is_some());
    }
}

#[test]
fn hold_delivery_refusal_does_not_claim_successful_rollback_on_io_failure() {
    let (_directory, store) = hold_fixture(SessionState::Ready);
    let (mut claimed, _) = claim_ready(&store, "session-test", &[]).unwrap();
    session::hold::change(&store, false).unwrap();
    let error = claimed.begin_delivery().unwrap_err();
    let token = claimed.token().to_owned();
    let failed =
        session::with_fault_budget(0, || claimed.settle_delivery(Delivery::NotSent(&error)));
    assert!(failed.is_err());
    drop(claimed);
    assert_eq!(store.status().unwrap().state, SessionState::Claimed);
    assert_eq!(
        current_claim_token(&store).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert_eq!(requests::list(&store).unwrap().receipts.len(), 1);
    assert!(store.events().unwrap().is_empty());
}

#[test]
fn hold_writer_waits_for_close_and_refuses_after_its_tombstone() {
    use std::sync::mpsc;
    let (_directory, store) = hold_fixture(SessionState::Ready);
    let directory = store.directory().to_owned();
    let (started, ready) = mpsc::channel();
    let lock = store.lock().unwrap();
    let writer = std::thread::spawn(move || {
        started.send(()).unwrap();
        session::hold::change(&Store::open_unchecked(directory), false)
    });
    ready.recv().unwrap();
    // Model close's terminal partial transition while it owns the lifecycle lock.
    let mut closed = store.status().unwrap();
    closed.state = SessionState::Closed;
    store.write_closed(&closed).unwrap();
    drop(lock);
    let error = writer.join().unwrap().err().unwrap();
    assert_eq!(
        error.to_string(),
        "the session is closed; a hold cannot be set or released"
    );
    assert!(store.record(CoreRecord::Hold).bytes().unwrap().is_none());
}

#[test]
fn released_hold_allows_a_new_follow_up_and_unknown_hold_refuses() {
    let (_directory, store) = hold_fixture(SessionState::Ready);
    store
        .record(CoreRecord::Hold)
        .write_private(b"malformed")
        .unwrap();
    let error = claim_ready(&store, "session-test", &[]).err().unwrap();
    assert_eq!(
        error.to_string(),
        "session session-test has an unreadable hold record; inspect it before sending a follow-up"
    );
    assert!(requests::list(&store).unwrap().receipts.is_empty());
    session::hold::change(&store, false).unwrap();
    assert!(claim_ready(&store, "session-test", &[]).is_err());
    session::hold::change(&store, true).unwrap();
    let (mut claimed, _) = claim_ready(&store, "session-test", &[]).unwrap();
    claimed.begin_delivery().unwrap();
    claimed.settle_delivery(Delivery::Sent).unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Working);
    // State refusal retains its exact original text even when also held.
    session::hold::change(&store, false).unwrap();
    let error = claim_ready(&store, "session-test", &[]).err().unwrap();
    assert_eq!(
        error.to_string(),
        "session session-test is working; tell requires the ready state"
    );
}

#[test]
fn report_failure_waits_for_lock_and_recovers_prior_completion() {
    use std::sync::mpsc;
    let (_directory, store) = fixture(SessionState::Working);
    let claimed = claim(&store, &[]).unwrap();
    let token = claimed.token().to_owned();
    let name = claimed.receipt().event_file.clone();
    claimed.retain();
    let lock = store.lock().unwrap();
    let mut pending =
        PendingTurnCompletion::new(&token, event("earlier completion"), None).unwrap();
    pending.event_file = name.clone();
    store.write_completion(&pending).unwrap();
    let directory = store.directory().to_owned();
    let (started, ready) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let store = Store::open_unchecked(directory);
        started.send(()).unwrap();
        let result = Report::for_claim(&store, FirstPartyCli::Codex, Some(&token)).fail(
            "late failure",
            None,
            None,
        );
        finished.send(result).unwrap();
    });
    ready.recv().unwrap();
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    drop(lock);
    done.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    writer.join().unwrap();
    assert_eq!(
        store.event_strict(&name).unwrap(),
        event("earlier completion")
    );
    assert_eq!(store.events().unwrap().len(), 1);
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert!(store.status().unwrap().error.is_none());
    assert!(
        store
            .record(CoreRecord::Completion)
            .bytes()
            .unwrap()
            .is_none()
    );
}

#[test]
fn failure_reports_keep_claim_until_event_sync_and_recover_before_late_failure() {
    for cancellation in [false, true] {
        let (_directory, store) = fixture(SessionState::Working);
        let claimed = claim(&store, &[]).unwrap();
        let token = claimed.token().to_owned();
        let receipt = claimed.receipt().clone();
        claimed.retain();
        if cancellation {
            store.record(CoreRecord::Cancel).write_json(&serde_json::json!({
                "schema":1, "request_id":receipt.request_id, "claim_token":token, "created_unix_ms":1
            })).unwrap();
        }
        let report = Report::for_claim(&store, FirstPartyCli::Pi, Some(&token));
        let error = with_sync_failure(store.record(CoreRecord::Events).path(), || {
            if cancellation {
                report.cancelled("interrupted", None, None)
            } else {
                report.fail("interrupted", None, None)
            }
        })
        .unwrap_err();
        assert!(injected_sync_failure(&error));
        assert_eq!(
            current_claim_token(&store).unwrap().as_deref(),
            Some(token.as_str())
        );
        assert_eq!(store.status().unwrap().state, SessionState::Working);
        let pending: PendingTurnCompletion = store.record(CoreRecord::Completion).json().unwrap();
        assert_eq!(pending.event.cancelled, cancellation);
        assert_eq!(
            store.event_strict(&receipt.event_file).unwrap(),
            pending.event
        );
        report
            .fail("later failure must not replace the journal", None, None)
            .unwrap();
        assert_eq!(
            store.event_strict(&receipt.event_file).unwrap(),
            pending.event
        );
        assert_eq!(store.events().unwrap().len(), 1);
        assert_eq!(store.status().unwrap().state, SessionState::Ready);
        assert_eq!(store.status().unwrap().error, pending.event.error);
        assert!(current_claim_token(&store).unwrap().is_none());
        assert!(
            store
                .record(CoreRecord::Completion)
                .bytes()
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn launch_fence_preserves_claim_status_and_receipt_on_drop() {
    for fenced in [false, true] {
        let (_directory, store) = fixture(SessionState::AwaitingInitialInput);
        update_status(
            store.directory(),
            SessionState::AwaitingInitialInput,
            None,
            Some("launch diagnostic".into()),
        )
        .unwrap();
        let mut claimed = claim(&store, &[]).unwrap();
        assert_eq!(claimed.rollback_state, None);
        let token = claimed.token().to_owned();
        let claim_bytes = store.record(CoreRecord::TurnClaim).bytes().unwrap();
        let status_bytes = store.record(CoreRecord::Status).bytes().unwrap();
        let receipt = claimed.receipt().clone();
        let receipt_path = store
            .record(CoreRecord::Requests)
            .path()
            .join(format!("{}.json", receipt.claim_token));
        let receipt_bytes = fs::read(&receipt_path).unwrap();
        if fenced {
            claimed.fence_launch();
        }
        drop(claimed);
        assert_eq!(
            store.record(CoreRecord::Status).bytes().unwrap(),
            status_bytes
        );
        assert_eq!(fs::read(receipt_path).unwrap(), receipt_bytes);
        assert!(store.events().unwrap().is_empty());
        assert!(
            store
                .record(CoreRecord::Completion)
                .bytes()
                .unwrap()
                .is_none()
        );
        if fenced {
            assert_eq!(
                store.record(CoreRecord::TurnClaim).bytes().unwrap(),
                claim_bytes
            );
            assert_eq!(current_claim_token(&store).unwrap(), Some(token));
            assert!(claim(&store, &[]).is_err());
        } else {
            assert!(current_claim_token(&store).unwrap().is_none());
            assert!(claim(&store, &[]).is_ok());
        }
    }
}

#[test]
fn startup_confirmation_restores_only_the_existing_drop_policy() {
    for confirmed in [false, true] {
        let (_directory, store) = fixture(SessionState::AwaitingInitialInput);
        let mut claimed = claim(&store, &[]).unwrap();
        let token = claimed.token().to_owned();
        let status_bytes = store.record(CoreRecord::Status).bytes().unwrap();
        let receipt_path = store
            .record(CoreRecord::Requests)
            .path()
            .join(format!("{}.json", claimed.token()));
        let receipt_bytes = fs::read(&receipt_path).unwrap();
        claimed.fence_launch();
        if confirmed {
            claimed.startup_confirmed();
        }
        assert_eq!(claimed.rollback_state, None);
        drop(claimed);
        assert_eq!(
            store.record(CoreRecord::Status).bytes().unwrap(),
            status_bytes
        );
        assert_eq!(fs::read(receipt_path).unwrap(), receipt_bytes);
        assert!(store.events().unwrap().is_empty());
        assert!(
            store
                .record(CoreRecord::Completion)
                .bytes()
                .unwrap()
                .is_none()
        );
        if confirmed {
            assert!(current_claim_token(&store).unwrap().is_none());
            assert!(claim(&store, &[]).is_ok());
        } else {
            assert_eq!(current_claim_token(&store).unwrap(), Some(token));
            assert!(claim(&store, &[]).is_err());
        }
    }
    let (_directory, store) = fixture(SessionState::Ready);
    let (mut claimed, _) = claim_ready(&store, "session-test", &[]).unwrap();
    assert_eq!(claimed.rollback_state, Some(SessionState::Ready));
    let receipt_path = store
        .record(CoreRecord::Requests)
        .path()
        .join(format!("{}.json", claimed.token()));
    let receipt_bytes = fs::read(&receipt_path).unwrap();
    claimed.fence_launch();
    claimed.startup_confirmed();
    assert_eq!(claimed.rollback_state, Some(SessionState::Ready));
    drop(claimed);
    assert!(current_claim_token(&store).unwrap().is_none());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
    assert_eq!(fs::read(receipt_path).unwrap(), receipt_bytes);
    assert!(store.events().unwrap().is_empty());
    assert!(
        store
            .record(CoreRecord::Completion)
            .bytes()
            .unwrap()
            .is_none()
    );
    assert!(claim_ready(&store, "session-test", &[]).is_ok());
}

#[test]
fn close_during_turn_claim_cannot_resurrect_the_session() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let waiter_directory = directory.path().to_owned();
    let waiter = thread::spawn(move || {
        claim_ready_with_callbacks(
            &waiter_directory,
            "session-closing123",
            || {},
            || {
                claimed_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                Ok(())
            },
            || {},
            &[],
        )
    });
    claimed_rx.recv().unwrap();

    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| panic!("no terminal handle"),
    )
    .map(|_| ())
    .unwrap();
    continue_tx.send(()).unwrap();
    let error = match waiter.join().unwrap() {
        Ok(_) => panic!("closed session unexpectedly reacquired a turn"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("claim disappeared"));
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn close_cannot_interleave_between_ready_validation_and_claimed_publication() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (publish_tx, publish_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let tell_directory = directory.path().to_owned();
    let tell = thread::spawn(move || {
        claim_ready_with_callbacks(
            &tell_directory,
            "session-closing123",
            || {},
            || Ok(()),
            || {
                publish_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
            },
            &[],
        )
    });
    publish_rx.recv().unwrap();
    let close_directory = directory.path().to_owned();
    let close = thread::spawn(move || {
        session_close::close(
            &Store::open_unchecked(&close_directory),
            None,
            session_close::budget::close_deadline(),
            |_| panic!("no terminal handle"),
        )
        .map(|_| ())
    });

    continue_tx.send(()).unwrap();
    let (claim, _) = tell.join().unwrap().unwrap();
    drop(claim);
    close.join().unwrap().unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn timeline_uses_the_existing_publication_predicate_at_every_completion_boundary() {
    for mutations in 0..=3 {
        let (directory, store) = hold_fixture(SessionState::Working);
        let claimed = claim(&store, &[]).unwrap();
        let request = claimed.receipt().request_id.clone();
        let token = claimed.token().to_owned();
        let event_path = store
            .record(CoreRecord::Events)
            .path()
            .join(&claimed.receipt().event_file);
        claimed.retain();
        let event = SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "same result".to_owned(),
            error: None,
            provider_session_id: Some("thread".to_owned()),
            turn_id: Some("turn".to_owned()),
            created_unix_ms: Some(5),
            usage: None,
            usage_error: None,
        };
        let mut pending = PendingTurnCompletion::new(&token, event, None).unwrap();
        pending.event_file = event_path.file_name().unwrap().to_str().unwrap().to_owned();
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if mutations >= 1 {
            write_completion_event(directory.path(), &pending).unwrap();
        }
        if mutations >= 2 {
            update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        }
        if mutations >= 3 {
            release_claim_token(&directory.path().join(TURN_CLAIM_FILE), &token).unwrap();
        }
        let before = snapshot_directory(directory.path());
        let value = query::timeline_value(
            &Reader::open_unchecked(directory.path()),
            "session-query",
            Some(&request),
        )
        .unwrap();
        assert_eq!(
            value["requests"][0]["request_state"],
            if mutations == 0 {
                "recovery_required"
            } else {
                "completed"
            }
        );
        assert_eq!(value["recovery_required"], true);
        assert_eq!(
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["stage"] == "completion" && e["record_state"] == "observed")
                .count(),
            usize::from(mutations >= 1)
        );
        assert_eq!(snapshot_directory(directory.path()), before);
    }
}

#[test]
fn an_oversized_completion_settles_identically_whichever_write_it_stops_after() {
    fn request_state(directory: &Path, request_id: &str) -> (String, String) {
        let value = query::request_result(&Reader::open_unchecked(directory), request_id).unwrap();
        (
            value["request_state"].as_str().unwrap().to_owned(),
            value["session_state"].as_str().unwrap().to_owned(),
        )
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

    fn settled_completion(
        directory: &Path,
        request_id: &str,
        event_path: &Path,
    ) -> SettledCompletion {
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        SettledCompletion {
            request: request_state(directory, request_id),
            status: (status.state.to_string(), status.error),
            event: read_json(event_path).unwrap(),
            claim_present: directory.join(TURN_CLAIM_FILE).exists(),
            journal_present: directory.join(TURN_COMPLETION_FILE).exists(),
        }
    }
    // The size policy is applied when the journal is created: a completion whose event
    // record would exceed the read limit is journaled as a failed completion whose error
    // names the size. Stopping the commit before every record write, then recovering, must
    // therefore reach one final state whether the event was written before the stop or not.
    const LIMIT: u64 = 4096;
    let event = event(&"x".repeat(LIMIT as usize));
    let size = serde_json::to_vec_pretty(&event).unwrap().len() as u64;
    assert!(size > LIMIT);
    let expected_error =
        format!("provider result of {size} bytes exceeds the {LIMIT} byte event limit");
    let mut journal_only = 0;
    let mut journal_and_event = 0;
    let mut settled = Vec::new();
    for budget in 0.. {
        let (_root, store) = hold_fixture(SessionState::Working);
        let directory = store.directory().to_owned();
        let claimed = claim(&store, &[]).unwrap();
        let request_id = claimed.receipt().request_id.clone();
        let claim_token = claimed.token().to_owned();
        let event_path = store
            .record(CoreRecord::Events)
            .path()
            .join(&claimed.receipt().event_file);
        claimed.retain();
        let claim_path = directory.join(TURN_CLAIM_FILE);
        let outcome = with_fault_budget(budget, || {
            let _lock = lock_turn_claim(&claim_path).unwrap();
            commit_completion_within_locked(
                &directory,
                &claim_path,
                &claim_token,
                event.clone(),
                None,
                SessionState::Ready,
                LIMIT,
            )
        });
        let interrupted = match outcome {
            Ok(()) => false,
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                true
            }
        };
        let label = format!("budget {budget}");
        let journal_present = directory.join(TURN_COMPLETION_FILE).exists();
        if journal_present {
            // The journal never holds a publishable record over the limit.
            let pending: PendingTurnCompletion =
                read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
            assert_eq!(pending.event.message, "", "{label}");
            assert_eq!(pending.status_state.as_str(), "failed", "{label}");
            assert_eq!(
                pending.event.error.as_deref(),
                Some(expected_error.as_str()),
                "{label}"
            );
            if event_path.exists() {
                journal_and_event += 1;
            } else {
                journal_only += 1;
            }
        } else if !event_path.exists() {
            // The stop came before the journal: nothing to recover and the claim stays.
            assert!(interrupted, "{label}");
            assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
            assert_eq!(
                request_state(&directory, &request_id),
                ("pending".to_owned(), "working".to_owned()),
                "{label}"
            );
            continue;
        }
        // Otherwise the journal was created (and possibly already settled and removed):
        // recovery must converge on the one final state.
        recover_pending_completion(&directory).unwrap();
        settled.push(settled_completion(&directory, &request_id, &event_path));
        assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
        if !interrupted {
            break;
        }
    }
    assert!(
        journal_only >= 1,
        "no stop between the journal and the event"
    );
    assert!(journal_and_event >= 1, "no stop after the event");
    let first = &settled[0];
    assert_eq!(first.request, ("failed".to_owned(), "failed".to_owned()));
    assert_eq!(
        first.status,
        ("failed".to_owned(), Some(expected_error.clone()))
    );
    assert_eq!(first.event.message, "");
    assert_eq!(first.event.error.as_deref(), Some(expected_error.as_str()));
    assert_eq!(
        first.event.provider_session_id.as_deref(),
        Some("provider-session")
    );
    assert_eq!(first.event.turn_id.as_deref(), Some("provider-turn"));
    assert!(!first.claim_present);
    assert!(!first.journal_present);
    for (index, state) in settled.iter().enumerate() {
        assert_eq!(state, first, "stop {index} settled differently");
    }
}

#[test]
fn cancel_reproduced_pre_cancel_writer_bytes_preserve_publication_and_recovery() {
    // Reproduced output of the pre-cancel writer at main commit 2947306, not a
    // runtime capture. EVENT is the literal form of tests/interrupted_close.rs's
    // event_text(). JOURNAL follows PendingTurnCompletion in session/turn.rs and
    // SessionEvent in native.rs, in their declared field order. Both use
    // session/primitives.rs::write_json_atomic: serde_json::to_vec_pretty, two-space
    // indentation, and no trailing newline. These sources were read with git show
    // 2947306:<path>; the separate main checkout was only read, never executed.
    // Neither fixture is generated by the current serializer or has dynamic ids.
    const EVENT: &[u8] = br#"{
  "provider": "codex",
  "message": "late result",
  "error": null,
  "provider_session_id": "thread",
  "turn_id": "turn-7",
  "created_unix_ms": 3
}"#;
    const JOURNAL: &[u8] = br#"{
  "schema": 1,
  "claim_token": "7-7-7",
  "event_file": "event-7.json",
  "event": {
    "provider": "codex",
    "message": "late result",
    "error": null,
    "provider_session_id": "thread",
    "turn_id": "turn-7",
    "created_unix_ms": 3
  },
  "status_error": null,
  "status_state": "ready"
}"#;
    for stage in 0..=3 {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("events")).unwrap();
        update_status(dir.path(), SessionState::Working, None, None).unwrap();
        let token = "7-7-7";
        write_private(&dir.path().join(TURN_CLAIM_FILE), token.as_bytes()).unwrap();
        let event: SessionEvent = serde_json::from_slice(EVENT).unwrap();
        let pending: PendingTurnCompletion = serde_json::from_slice(JOURNAL).unwrap();
        assert!(!event.cancelled);
        assert_eq!(serde_json::to_vec_pretty(&event).unwrap(), EVENT);
        assert_eq!(serde_json::to_vec_pretty(&pending).unwrap(), JOURNAL);
        write_private(&dir.path().join(TURN_COMPLETION_FILE), JOURNAL).unwrap();
        if stage >= 1 {
            write_private(&dir.path().join("events/event-7.json"), EVENT).unwrap();
        }
        if stage >= 2 {
            update_status(dir.path(), SessionState::Ready, None, None).unwrap();
        }
        if stage >= 3 {
            release_claim_token(&dir.path().join(TURN_CLAIM_FILE), token).unwrap();
        }
        let reader = Reader::open_unchecked(dir.path());
        let read = reader
            .journaled_event_state_within(&pending, EVENT_READ_LIMIT)
            .unwrap();
        assert_eq!(
            session::turn::event_published(true, Some(&read), stage < 3),
            stage >= 1
        );
        recover_pending_completion(dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("events/event-7.json")).unwrap(),
            EVENT
        );
        assert!(!dir.path().join(TURN_CLAIM_FILE).exists());
    }
}

#[test]
fn stale_turn_claim_cannot_release_a_new_owner() {
    let directory = tempfile::tempdir().unwrap();
    let stale = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    {
        let store = Store::open_unchecked(directory.path());
        let _lock = store.lock().unwrap();
        remove_turn_claim_locked(store.record(CoreRecord::TurnClaim).path()).unwrap();
    }
    let current = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();

    drop(stale);

    assert!(turn::claim(&Store::open_unchecked(directory.path()), &[]).is_err());
    drop(current);
    assert!(turn::claim(&Store::open_unchecked(directory.path()), &[]).is_ok());
}

#[test]
fn immutable_request_mapping_survives_each_partial_completion_recovery() {
    for completed_mutations in 0..=3 {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        let receipt = claim.receipt().clone();
        let token = claim.token().to_owned();
        claim.retain();
        let event = SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "codex".to_owned(),
            message: "same text".to_owned(),
            error: None,
            provider_session_id: Some("thread".to_owned()),
            turn_id: Some("turn".to_owned()),
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        };
        let mut pending = PendingTurnCompletion::new(&token, event, None).unwrap();
        pending.event_file = receipt.event_file.clone();
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if completed_mutations >= 1 {
            write_completion_event(directory.path(), &pending).unwrap();
        }
        if completed_mutations >= 2 {
            update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        }
        if completed_mutations >= 3 {
            release_claim_token(&directory.path().join(TURN_CLAIM_FILE), &token).unwrap();
        }
        recover_pending_completion(directory.path()).unwrap();
        let recovered = requests::for_claim(&Reader::open_unchecked(directory.path()), &token)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.request_id, receipt.request_id);
        assert_eq!(recovered.event_file, receipt.event_file);
        let stored: SessionEvent =
            read_json(&directory.path().join("events").join(recovered.event_file)).unwrap();
        assert_eq!(stored, pending.event);
    }
}

#[test]
fn refusal_release_is_fenced_by_claim_ownership_without_publishing_events() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    fs::create_dir(directory.join("events")).unwrap();
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (mut refused, _) =
        turn::claim_ready(&Store::open_unchecked(directory), "session-test", &[]).unwrap();
    let refused_request = refused.receipt().request_id.clone();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");

    // The refused claim is released out from under the refusal before it is recorded, and
    // the next `tell` claims the turn and starts working.
    {
        let store = Store::open_unchecked(directory);
        let _lock = store.lock().unwrap();
        remove_turn_claim_locked(store.record(CoreRecord::TurnClaim).path()).unwrap();
    }
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (next, _) =
        turn::claim_ready(&Store::open_unchecked(directory), "session-test", &[]).unwrap();
    let next_token = next.token().to_owned();
    let next_request = next.receipt().request_id.clone();
    update_status(directory, SessionState::Working, None, None).unwrap();

    let reported = format!("{refusal:#}");
    assert!(
        !refused
            .release_now_with_reason(SessionState::Ready, reported.clone())
            .unwrap()
    );
    drop(refused);

    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working", "{status:?}");
    assert_eq!(status.error, None, "{status:?}");
    assert_eq!(
        fs::read_to_string(directory.join(TURN_CLAIM_FILE))
            .unwrap()
            .trim(),
        next_token
    );
    let receipts = requests::list(&Reader::open_unchecked(directory)).unwrap();
    assert_eq!(receipts.unreadable, 0);
    let mut recorded: Vec<&str> = receipts
        .receipts
        .iter()
        .map(|receipt| receipt.request_id.as_str())
        .collect();
    recorded.sort_unstable();
    let mut expected = [refused_request.as_str(), next_request.as_str()];
    expected.sort_unstable();
    assert_eq!(recorded, expected);
    for receipt in &receipts.receipts {
        assert!(
            !directory.join("events").join(&receipt.event_file).exists(),
            "{}: no result was recorded for either request",
            receipt.request_id
        );
    }
    next.retain();
    assert!(directory.join(TURN_CLAIM_FILE).is_file());

    // With the turn still its own, the refusal releases the claim and publishes its reason
    // in the same write.
    {
        let store = Store::open_unchecked(directory);
        let _lock = store.lock().unwrap();
        remove_turn_claim_locked(store.record(CoreRecord::TurnClaim).path()).unwrap();
    }
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (mut refused, _) =
        turn::claim_ready(&Store::open_unchecked(directory), "session-test", &[]).unwrap();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");
    let reported = format!("{refusal:#}");
    assert!(
        refused
            .release_now_with_reason(SessionState::Ready, reported.clone())
            .unwrap()
    );
    drop(refused);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready", "{status:?}");
    assert_eq!(
        status.error.as_deref(),
        Some(format!("{reported:#}").as_str())
    );
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
}

fn sample_usage() -> crate::native::session::schema::ProviderUsage {
    crate::native::session::schema::ProviderUsage {
        source: "pi_extension_agent_end".to_owned(),
        scope: "request".to_owned(),
        coverage: "complete".to_owned(),
        runs: 1,
        assistant_messages: 2,
        input_tokens: 793,
        output_tokens: 58,
        cache_read_tokens: 16640,
        cache_write_tokens: 0,
        reasoning_tokens: Some(23),
        total_tokens: 17491,
    }
}

#[test]
fn size_policy_drops_usage_before_it_fails_a_message_that_fits_on_its_own() {
    use crate::native::session::schema::USAGE_OMITTED_SIZE;
    let mut with_usage = event("fits");
    with_usage.usage = Some(sample_usage());
    let without_usage = event("fits");
    let bare = serde_json::to_vec_pretty(&without_usage).unwrap().len() as u64;
    let full = serde_json::to_vec_pretty(&with_usage).unwrap().len() as u64;
    assert!(full > bare);
    let pending =
        |event: &SessionEvent| PendingTurnCompletion::new("1-2-3", event.clone(), None).unwrap();
    // Room for everything: the usage is kept.
    let kept = bound_pending_completion(pending(&with_usage), full).unwrap();
    assert_eq!(kept.event, with_usage);
    assert_eq!(kept.status_state, SessionState::Ready);
    // Room for the message but not the usage: the completion is recorded without usage.
    let dropped = bound_pending_completion(pending(&with_usage), full - 1).unwrap();
    assert_eq!(dropped.event.usage, None);
    assert_eq!(
        dropped.event.usage_error.as_deref(),
        Some(USAGE_OMITTED_SIZE)
    );
    assert_eq!(dropped.event.message, "fits");
    assert_eq!(dropped.event.error, None);
    assert_eq!(dropped.status_state, SessionState::Ready);
    assert_eq!(dropped.status_error, None);
    let mut expected = without_usage.clone();
    expected.usage_error = Some(USAGE_OMITTED_SIZE.to_owned());
    assert_eq!(dropped.event, expected);
    // The message exceeds the limit on its own: the existing size failure, named by the
    // size of the bare event, with both usage fields absent.
    let failed = bound_pending_completion(pending(&with_usage), bare - 1).unwrap();
    let error = format!(
        "provider result of {bare} bytes exceeds the {} byte event limit",
        bare - 1
    );
    assert_eq!(failed.event.message, "");
    assert_eq!(failed.event.error.as_deref(), Some(error.as_str()));
    assert_eq!(failed.status_error.as_deref(), Some(error.as_str()));
    assert_eq!(failed.status_state, SessionState::Failed);
    assert_eq!(failed.event.usage, None);
    assert_eq!(failed.event.usage_error, None);
    // A completion without usage behaves exactly as before.
    let plain = bound_pending_completion(pending(&without_usage), bare - 1).unwrap();
    assert_eq!(plain.status_state, SessionState::Failed);
    assert_eq!(
        plain.event.error.as_deref(),
        Some(
            format!(
                "provider result of {bare} bytes exceeds the {} byte event limit",
                bare - 1
            )
            .as_str()
        )
    );
    assert_eq!(plain.event.usage_error, None);
}

#[test]
fn size_policy_publishes_a_bare_event_at_the_limit_before_it_fails() {
    use crate::native::query::reported_usage;
    use crate::native::session::schema::USAGE_OMITTED_SIZE;
    let without_usage = event("fits");
    let bare = serde_json::to_vec_pretty(&without_usage).unwrap().len() as u64;
    let pending =
        |event: &SessionEvent| PendingTurnCompletion::new("1-2-3", event.clone(), None).unwrap();
    // The bare event is exactly the limit; the usage pushes it over, and so does the
    // omission marker that replaces it. The event is published bare, not failed.
    let mut with_usage = event("fits");
    with_usage.usage = Some(sample_usage());
    let published = bound_pending_completion(pending(&with_usage), bare).unwrap();
    assert_eq!(published.event, without_usage);
    assert_eq!(published.status_state, SessionState::Ready);
    assert_eq!(published.status_error, None);
    assert_eq!(
        reported_usage(Some(&published.event)),
        (None, Some("not_reported"), None)
    );
    // The only extra is a malformed-usage error the writer recorded: it is dropped the same
    // way, and never turns a message that fits on its own into a failure.
    let mut with_error = event("fits");
    with_error.usage_error = Some("malformed usage: total_tokens".to_owned());
    let with_error_size = serde_json::to_vec_pretty(&with_error).unwrap().len() as u64;
    assert!(with_error_size > bare);
    let published = bound_pending_completion(pending(&with_error), bare).unwrap();
    assert_eq!(published.event, without_usage);
    assert_eq!(published.status_state, SessionState::Ready);
    // With room for the marker but not the malformed error, the marker is recorded.
    let mut marked = without_usage.clone();
    marked.usage_error = Some(USAGE_OMITTED_SIZE.to_owned());
    let marked_size = serde_json::to_vec_pretty(&marked).unwrap().len() as u64;
    assert!(marked_size < with_error_size);
    let omitted = bound_pending_completion(pending(&with_error), marked_size).unwrap();
    assert_eq!(omitted.event, marked);
    // Below the bare size the failure record carries neither usage field.
    let failed = bound_pending_completion(pending(&with_error), bare - 1).unwrap();
    assert_eq!(failed.status_state, SessionState::Failed);
    assert_eq!(failed.event.message, "");
    assert_eq!(
        failed.event.error.as_deref(),
        Some(
            format!(
                "provider result of {bare} bytes exceeds the {} byte event limit",
                bare - 1
            )
            .as_str()
        )
    );
    assert_eq!(failed.event.usage, None);
    assert_eq!(failed.event.usage_error, None);
}

#[test]
fn journal_predicate_accepts_legacy_event_bytes_and_round_trips_usage() {
    // An event this binary writes without usage has the bytes an older writer produced:
    // no usage field appears, so an old journal and its old event read as Committed.
    let legacy = event("legacy");
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    assert!(!String::from_utf8_lossy(&legacy_bytes).contains("usage"));
    let old_journal: PendingTurnCompletion = serde_json::from_str(&format!(
        r#"{{"schema":1,"claim_token":"1-2-3","event_file":"event-1-1.json","event":{},"status_error":null}}"#,
        String::from_utf8(legacy_bytes.clone()).unwrap()
    ))
    .unwrap();
    assert_eq!(old_journal.event.usage, None);
    assert_eq!(old_journal.event.usage_error, None);
    assert_eq!(
        journaled_event_state_of(&old_journal, &legacy_bytes, EVENT_READ_LIMIT).unwrap(),
        JournaledEventState::Committed
    );
    // The strict decoders read a legacy event and an event with usage alike.
    let decoded: SessionEvent = crate::native::session::record::decode(&legacy_bytes).unwrap();
    assert_eq!(decoded, legacy);
    let mut with_usage = event("usage");
    with_usage.usage = Some(sample_usage());
    let (_root, store) = fixture(SessionState::Working);
    let pending = PendingTurnCompletion::new("1-2-3", with_usage.clone(), None).unwrap();
    let path = store
        .record(CoreRecord::Events)
        .path()
        .join(&pending.event_file);
    RecordStore::at(&path).write_json(&pending.event).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(
        journaled_event_state_of(&pending, &bytes, EVENT_READ_LIMIT).unwrap(),
        JournaledEventState::Committed
    );
    let decoded: SessionEvent = crate::native::session::record::decode(&bytes).unwrap();
    assert_eq!(decoded, with_usage);
    assert_eq!(
        Reader::open_unchecked(store.directory())
            .event_strict(&pending.event_file)
            .unwrap(),
        with_usage
    );
    // An older binary would drop the fields and see Mismatched: the declared no-downgrade
    // boundary while a session's turn is journaled.
    let mut dropped = pending.event.clone();
    dropped.usage = None;
    assert_ne!(serde_json::to_vec_pretty(&dropped).unwrap(), bytes);
}

#[test]
fn report_usage_never_changes_the_recorded_outcome() {
    for (kind, cancelled_flag) in [("complete", false), ("fail", false), ("cancelled", false)] {
        let (_root, store) = hold_fixture(SessionState::Working);
        let claimed = claim(&store, &[]).unwrap();
        let token = claimed.token().to_owned();
        claimed.retain();
        let report = Report::for_claim(&store, FirstPartyCli::Pi, Some(&token))
            .with_usage(Some(sample_usage()), None);
        match kind {
            "complete" => report.complete("done", None, None).unwrap(),
            "fail" => report.fail("broken", None, None).unwrap(),
            _ => report.cancelled("stopped", None, None).unwrap(),
        }
        let paths = crate::native::session::primitives::event_paths(store.directory()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.usage, Some(sample_usage()), "{kind}");
        assert_eq!(event.usage_error, None, "{kind}");
        // No cancel intent was recorded, so `cancelled` is an ordinary failure here.
        assert_eq!(event.cancelled, cancelled_flag, "{kind}");
        assert_eq!(event.error.is_some(), kind != "complete", "{kind}");
        let request_id = requests::list(&Reader::open_unchecked(store.directory()))
            .unwrap()
            .receipts[0]
            .request_id
            .clone();
        let value =
            query::request_result(&Reader::open_unchecked(store.directory()), &request_id).unwrap();
        assert_eq!(value["usage"]["total_tokens"], 17491, "{kind}");
        assert_eq!(value["usage_reason"], serde_json::Value::Null, "{kind}");
        assert_eq!(
            value["request_state"],
            if kind == "complete" {
                "completed"
            } else {
                "failed"
            },
            "{kind}"
        );
    }
}

#[test]
fn provenance_is_optional_byte_compatible_and_old_journal_readers_lose_it() {
    let legacy = br#"{"provider":"codex","message":"legacy","error":null,"provider_session_id":"provider-session","turn_id":"provider-turn","created_unix_ms":1}"#;
    let decoded: SessionEvent = serde_json::from_slice(legacy).unwrap();
    assert_eq!(decoded.provenance, None);
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), legacy);
    let mut marked = decoded;
    marked.provenance = Some("codex_notify".into());
    let journal = PendingTurnCompletion::new("1-2-3", marked, None).unwrap();
    let bytes = serde_json::to_vec_pretty(&journal.event).unwrap();
    let encoded = serde_json::to_vec(&journal).unwrap();
    let decoded: PendingTurnCompletion = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        journaled_event_state_of(&decoded, &bytes, EVENT_READ_LIMIT).unwrap(),
        JournaledEventState::Committed
    );
    // Actual pre-provenance event shape (unknown fields ignored), inside a pending journal.
    #[derive(Deserialize, Serialize)]
    struct OldEvent {
        provider: String,
        message: String,
        error: Option<String>,
        provider_session_id: Option<String>,
        turn_id: Option<String>,
        created_unix_ms: Option<u128>,
    }
    #[derive(Deserialize)]
    struct OldJournal {
        event: OldEvent,
    }
    let old: OldJournal = serde_json::from_slice(&encoded).unwrap();
    let mut dropped: PendingTurnCompletion = serde_json::from_slice(&encoded).unwrap();
    dropped.event = serde_json::from_value(serde_json::to_value(old.event).unwrap()).unwrap();
    assert_eq!(
        journaled_event_state_of(&dropped, &bytes, EVENT_READ_LIMIT).unwrap(),
        JournaledEventState::Mismatched
    );
}

#[test]
fn provenance_survives_each_completion_publication_fault_boundary() {
    let mut journal_only = false;
    let mut journal_and_event = false;
    for budget in 0..40 {
        let (_root, store) = hold_fixture(SessionState::Working);
        let claimed = claim(&store, &[]).unwrap();
        let token = claimed.token().to_owned();
        let request_id = claimed.receipt().request_id.clone();
        claimed.retain();
        let outcome = with_fault_budget(budget, || {
            Report::for_claim(&store, FirstPartyCli::Codex, Some(&token))
                .with_provenance(Some("codex_notify"))
                .complete("done", Some("thread".into()), Some("turn".into()))
        });
        if let Err(error) = &outcome {
            assert!(injected_fault(error), "{error:#}");
        }
        let reader = Reader::open_unchecked(store.directory());
        let journal: Option<PendingTurnCompletion> = reader
            .record(CoreRecord::Completion)
            .optional_json()
            .unwrap();
        if let Some(journal) = journal {
            assert_eq!(journal.event.provenance.as_deref(), Some("codex_notify"));
            if reader.events().unwrap().is_empty() {
                journal_only = true;
            } else {
                journal_and_event = true;
            }
        } else if reader.events().unwrap().is_empty() {
            continue;
        }
        recover_pending_completion(store.directory()).unwrap();
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 1);
        let event: SessionEvent = read_json(&events[0]).unwrap();
        assert_eq!(event.provenance.as_deref(), Some("codex_notify"));
        let result = query::request_result(&reader, &request_id).unwrap();
        assert_eq!(result["request_state"], "completed");
        assert_eq!(
            reader
                .event_strict(events[0].file_name().unwrap().to_str().unwrap())
                .unwrap()
                .provenance,
            event.provenance
        );
        if outcome.is_ok() {
            assert!(journal_only && journal_and_event);
            return;
        }
    }
    panic!("publication did not finish within the fault sweep");
}

#[test]
fn stale_and_duplicate_reports_never_rewrite_winning_provenance_in_either_order() {
    for (first, second) in [
        ("codex_notify", "codex_transcript_task_complete"),
        ("codex_transcript_task_complete", "codex_notify"),
    ] {
        for failure in [false, true] {
            let (_root, store) = fixture(SessionState::Working);
            let claim = claim(&store, &[]).unwrap();
            let report = Report::for_claim(&store, FirstPartyCli::Codex, Some(claim.token()))
                .with_provenance(Some(first));
            if failure {
                report
                    .fail("failed", Some("thread".into()), Some("turn".into()))
                    .unwrap();
            } else {
                report
                    .complete("done", Some("thread".into()), Some("turn".into()))
                    .unwrap();
            }
            let reader = Reader::open_unchecked(store.directory());
            let path = reader.events().unwrap().pop().unwrap();
            let before = fs::read(&path).unwrap();
            Report::for_claim(&store, FirstPartyCli::Codex, Some(claim.token()))
                .with_provenance(Some(second))
                .complete("stale", Some("thread".into()), Some("turn".into()))
                .unwrap();
            let next = turn::claim(&store, &[]).unwrap();
            update_status(store.directory(), SessionState::Claimed, None, None).unwrap();
            update_status(store.directory(), SessionState::Working, None, None).unwrap();
            Report::for_claim(&store, FirstPartyCli::Codex, Some(next.token()))
                .with_provenance(Some(second))
                .fail(
                    "duplicate identity",
                    Some("thread".into()),
                    Some("turn".into()),
                )
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(reader.events().unwrap().len(), 1);
            assert_eq!(
                reader
                    .event_strict(path.file_name().unwrap().to_str().unwrap())
                    .unwrap()
                    .provenance
                    .as_deref(),
                Some(first)
            );
        }
    }
}
