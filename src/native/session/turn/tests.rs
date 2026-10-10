use super::*;

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
        cancelled: false,
        provider: "codex".to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: Some(1),
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
                session::close::close(&store, None, |_| panic!("no bound surface")).unwrap();
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
            session::close::close(&store, None, |_| panic!("no surface")).unwrap();
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
    let result = acquire_ready_turn_claim_after_claim(store.directory(), "session-test", || {
        session::hold::change(&store, false)?;
        Ok(())
    });
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
