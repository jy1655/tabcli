use super::*;
use crate::native::session::{CoreRecord, SessionState, update_status};
use std::fs;

#[cfg(windows)]
fn delivery_without_pi_event(initial: bool) {
    use crate::native::delivery::{DeliveryContext, Turn, deliver_with, test_support::Fixture};
    use std::{cell::Cell, rc::Rc};
    let fixture = Fixture::new();
    let claim = fixture.claim(initial);
    let token = claim.token().to_owned();
    install_pending_turn(fixture.store.directory(), &token).unwrap();
    fixture
        .store
        .private(STARTUP_READY_FILE)
        .write_json(&serde_json::json!({
            "schema": 2, "claim_token": token, "session_id": "pi-input",
            "input_ack_schema": 1,
        }))
        .unwrap();
    let sends = Rc::new(Cell::new(0));
    let count = sends.clone();
    let result = super::test_support::with_console_send(
        Box::new(move |path| {
            count.set(count.get() + 1);
            let frame = fs::read_to_string(path).unwrap();
            assert!(frame.contains("agent-bridge-pi-turn:"));
            assert!(!frame.contains('\n'));
            Ok(())
        }),
        || {
            let mut gate = |_| Ok(());
            deliver_with(
                claim,
                if initial {
                    Turn::Initial { gate: &mut gate }
                } else {
                    Turn::FollowUp { prompt: "prompt" }
                },
                DeliveryContext {
                    deadline: Instant::now() + Duration::from_millis(150),
                    ..fixture.context()
                },
                if initial { &PiInitialPaste } else { &PiPaste },
                &fixture.checks,
            )
        },
    );
    assert_eq!(sends.get(), 1);
    assert!(
        result.is_err(),
        "console batch success alone must remain Uncertain, got {result:?}"
    );
    assert_eq!(
        turn::current_claim_token(&fixture.store)
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    let status = fixture.store.status().unwrap();
    assert_eq!(status.state, SessionState::Working);
    assert!(status.error.unwrap().contains("Pi"));
    assert!(fixture.store.private(PENDING_TURN_FILE).path().exists());
    assert_eq!(
        fixture
            .store
            .record(CoreRecord::InitialPrompt)
            .path()
            .exists(),
        initial
    );
}

#[test]
#[cfg(windows)]
fn input_ack_initial_console_success_without_pi_event_is_uncertain() {
    delivery_without_pi_event(true);
}

#[test]
#[cfg(windows)]
fn input_ack_followup_console_success_without_pi_event_is_uncertain() {
    delivery_without_pi_event(false);
}

#[test]
fn input_ack_capability_survives_two_followups_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    store
        .private(STARTUP_READY_FILE)
        .write_json(&serde_json::json!({
            "schema": 2, "claim_token": pending.claim_token, "session_id": "pi-input",
            "input_ack_schema": 1,
        }))
        .unwrap();
    for _ in 0..2 {
        store.record(CoreRecord::TurnClaim).remove().unwrap();
        let next = turn::claim(&store, &[]).unwrap();
        let token = next.token().to_owned();
        next.retain();
        install_pending_turn(directory.path(), &token).unwrap();
        cancel_pending_turn(directory.path(), &token).unwrap();
        let ready: serde_json::Value = store.private(STARTUP_READY_FILE).json().unwrap();
        assert_eq!(
            ready["input_ack_schema"], 1,
            "carry-forward must retain the extension's declared capability"
        );
        assert_eq!(ready["claim_token"], token);
        assert_eq!(ready["session_id"], "pi-input");
    }
}

#[test]
fn input_ack_custom_event_clears_old_result_and_usage_correlation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("extension.js"), bridge_extension()).unwrap();
    fs::write(root.join("test.mjs"), include_str!("../pi_custom_test.mjs")).unwrap();
    let mut inputs = Vec::new();
    for _ in 0..2 {
        let store = Store::open_unchecked(root);
        store.record(CoreRecord::TurnClaim).remove().unwrap();
        update_status(root, SessionState::Working, None, None).unwrap();
        let pending = super::test_support::claim_pending_turn(root);
        inputs.push(serde_json::json!({
            "claim": fs::read_to_string(store.record(CoreRecord::TurnClaim).path()).unwrap(),
            "token": pending.claim_token,
            "prompt": correlated_prompt("synthetic input", &pending),
        }));
    }
    Store::open_unchecked(root)
        .private("inputs.json")
        .write_json(&inputs)
        .unwrap();
    let output = std::process::Command::new("node")
        .arg(root.join("test.mjs"))
        .arg(root)
        .output()
        .expect("Node on PATH is required to execute the Pi extension tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
