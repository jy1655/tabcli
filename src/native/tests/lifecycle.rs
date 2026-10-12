use super::support::*;
use super::*;

#[test]
fn provider_failures_finish_the_bridge_turn_without_reporting_success() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();

    turn::Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Pi,
        None,
    )
    .fail(
        "Pi turn aborted",
        Some("provider-session".to_owned()),
        Some("provider-turn".to_owned()),
    )
    .unwrap();

    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
    assert_eq!(status.error.as_deref(), Some("Pi turn aborted"));
    let error = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        None,
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::from_secs(1))
            .unwrap(),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("Pi turn aborted"));
}

#[test]
fn monitor_failures_are_journaled_against_the_current_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();

    turn::Report::monitor_failure(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Pi,
        "Pi result monitor stopped",
    )
    .unwrap();

    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "failed");
    assert_eq!(status.error.as_deref(), Some("Pi result monitor stopped"));
    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.error.as_deref(), Some("Pi result monitor stopped"));
}

#[test]
fn terminal_status_cannot_regress_and_generation_is_monotonic() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
    let running: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    update_status(directory.path(), SessionState::Exited, Some(0), None).unwrap();
    let exited: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    assert!(exited.generation > running.generation);
    assert!(update_status(directory.path(), SessionState::Ready, None, None).is_err());
    let stable: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(stable.state.as_str(), "exited");
    assert_eq!(stable.generation, exited.generation);
}

#[test]
fn claimed_session_can_finalize_when_the_provider_exits_before_delivery() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();

    finalize_native_session(directory.path(), &Ok(())).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "exited");
    assert_eq!(status.exit_code, Some(0));
}

#[test]
fn native_finalization_is_idempotent_after_a_terminal_status() {
    for state in ["exited", "failed", "closed"] {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        if state != "closed" {
            update_status(directory.path(), SessionState::Running, None, None).unwrap();
        }
        match state {
            "exited" => {
                update_status(directory.path(), state.parse().unwrap(), Some(0), None).unwrap()
            }
            "failed" => update_status(
                directory.path(),
                state.parse().unwrap(),
                None,
                Some("provider failed".to_owned()),
            )
            .unwrap(),
            "closed" => {
                update_status(directory.path(), state.parse().unwrap(), None, None).unwrap()
            }
            _ => unreachable!(),
        }
        let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

        finalize_native_session(directory.path(), &Ok(())).unwrap();

        let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(after.state.as_str(), state);
        assert_eq!(after.generation, before.generation);
    }
}

#[test]
fn completion_and_process_exit_converge_in_either_serialized_order() {
    for completion_first in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        update_status(directory.path(), SessionState::Running, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        let claim_token = claim.token().to_owned();
        claim.retain();

        let complete = || {
            turn::Report::for_claim(
                &Store::open_unchecked(directory.path()),
                FirstPartyCli::Codex,
                Some(&claim_token),
            )
            .complete(
                "completed result",
                Some("codex-session".to_owned()),
                Some("codex-turn".to_owned()),
            )
            .unwrap();
        };
        if completion_first {
            complete();
            finalize_native_session(directory.path(), &Ok(())).unwrap();
        } else {
            finalize_native_session(directory.path(), &Ok(())).unwrap();
            complete();
        }

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "exited");
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert_eq!(
            event_paths(directory.path()).unwrap().len(),
            usize::from(completion_first)
        );
    }
}

#[test]
fn duplicate_provider_turn_cannot_release_a_replacement_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let first_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    first_claim.retain();
    turn::Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Codex,
        None,
    )
    .complete(
        "first result",
        Some("codex-session".to_owned()),
        Some("codex-turn".to_owned()),
    )
    .unwrap();
    let replacement_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    replacement_claim.retain();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    turn::Report::for_claim(
        &Store::open_unchecked(directory.path()),
        FirstPartyCli::Codex,
        None,
    )
    .complete(
        "duplicate result",
        Some("codex-session".to_owned()),
        Some("codex-turn".to_owned()),
    )
    .unwrap();

    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

#[test]
fn status_updates_replace_atomically() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
}

#[test]
fn claim_release_waits_for_the_lifecycle_lock_before_deleting() {
    let directory = tempfile::tempdir().unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let path = directory.path().join(TURN_CLAIM_FILE);
    let lifecycle_lock = lock_turn_claim(&path).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();

    let release = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        drop(claim);
        finished_tx.send(()).unwrap();
    });
    started_rx.recv().unwrap();
    assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());

    drop(lifecycle_lock);
    finished_rx.recv().unwrap();
    release.join().unwrap();
    assert!(!path.exists());
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TurnEvent {
    HookCompletion,
    MonitorFailure,
    ProcessExit,
    DelayedDeliveryFailure,
}

fn permutations<T: Copy>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for (index, first) in items.iter().enumerate() {
        let mut rest = items.to_vec();
        rest.remove(index);
        for mut tail in permutations(&rest) {
            tail.insert(0, *first);
            all.push(tail);
        }
    }
    all
}

#[test]
fn turn_lifecycle_converges_under_every_completion_exit_and_failure_order() {
    let events = [
        TurnEvent::HookCompletion,
        TurnEvent::MonitorFailure,
        TurnEvent::ProcessExit,
        TurnEvent::DelayedDeliveryFailure,
    ];
    let orders = permutations(&events);
    assert_eq!(orders.len(), 24);
    for order in orders {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        update_status(directory.path(), SessionState::Running, None, None).unwrap();
        update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        let mut claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        claim.settle_delivery(turn::Delivery::Sent).unwrap();
        update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let mut previous: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

        for event in &order {
            let claim_present = directory.path().join(TURN_CLAIM_FILE).exists();
            match event {
                TurnEvent::HookCompletion => turn::Report::for_claim(
                    &Store::open_unchecked(directory.path()),
                    FirstPartyCli::Codex,
                    Some(claim.token()),
                )
                .complete(
                    "hook result",
                    Some("codex-session".to_owned()),
                    Some("codex-turn".to_owned()),
                )
                .unwrap(),
                TurnEvent::MonitorFailure => turn::Report::monitor_failure(
                    &Store::open_unchecked(directory.path()),
                    FirstPartyCli::Codex,
                    "monitor stopped",
                )
                .unwrap(),
                TurnEvent::ProcessExit => {
                    finalize_native_session(directory.path(), &Ok(())).unwrap()
                }
                TurnEvent::DelayedDeliveryFailure => {
                    let failure = delivery_uncertain_failure("paste timed out");
                    let _ = claim.settle_delivery(turn::Delivery::Uncertain(failure.error()));
                }
            }
            let current: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert!(
                valid_status_transition(&previous.state, &current.state),
                "{order:?}: {event:?} moved {} -> {}",
                previous.state,
                current.state
            );
            assert!(
                current.generation >= previous.generation,
                "{order:?}: {event:?}"
            );
            if *event == TurnEvent::DelayedDeliveryFailure && !claim_present {
                assert_eq!(current.generation, previous.generation, "{order:?}");
                assert_eq!(current.error, previous.error, "{order:?}");
            }
            assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
            previous = current;
        }

        assert!(
            !directory.path().join(TURN_CLAIM_FILE).exists(),
            "{order:?}"
        );
        match previous.state.as_str() {
            "exited" => assert_eq!(previous.error, None, "{order:?}"),
            "failed" => assert_eq!(
                previous.error.as_deref(),
                Some("monitor stopped"),
                "{order:?}"
            ),
            other => panic!("{order:?} ended in {other}"),
        }
        let stored = event_paths(directory.path())
            .unwrap()
            .iter()
            .map(|path| read_json::<SessionEvent>(path).unwrap())
            .collect::<Vec<_>>();
        assert!(stored.len() <= 2, "{order:?}: {stored:?}");
        assert!(
            stored
                .iter()
                .filter(|event| event.message == "hook result")
                .count()
                <= 1,
            "{order:?}"
        );
        assert!(
            stored
                .iter()
                .all(|event| event.error.as_deref() != Some("paste timed out")),
            "{order:?}"
        );
    }
}
