use super::super::test_support::{claim_pending_turn, marked};
use super::super::{ADAPTER, PENDING_TURN_FILE};
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::{SessionState, schema::SessionEvent};
use crate::native::{TURN_CLAIM_FILE, event_paths, read_json, update_status, write_json_atomic};
use std::fs;

#[test]
fn cancel_hook_requires_correlated_token_and_cancel_intent() {
    for detail in [
        "Pi turn ended with aborted",
        "Pi turn ended with error: This operation was aborted",
    ] {
        for (evidence, correlated, expected) in [
            ("matching", true, Some(true)),
            ("missing_token", true, Some(false)),
            ("wrong_token", true, Some(false)),
            ("wrong_cancel", true, Some(false)),
            ("missing_cancel", true, Some(false)),
            ("malformed_cancel", true, Some(false)),
            ("matching", false, None),
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join("events")).unwrap();
            update_status(directory.path(), SessionState::Working, None, None).unwrap();
            let pending = claim_pending_turn(directory.path());
            let cancel_token = if evidence == "wrong_cancel" {
                "1-2-3"
            } else {
                &pending.claim_token
            };
            if evidence != "missing_cancel" {
                write_json_atomic(
                    &directory.path().join("cancel.json"),
                    &serde_json::json!({
                        "schema": 1, "request_id": "request-test",
                        "claim_token": cancel_token, "created_unix_ms": 1,
                    }),
                )
                .unwrap();
            }
            if evidence == "malformed_cancel" {
                fs::write(directory.path().join("cancel.json"), "not-json").unwrap();
            }
            let mut payload = serde_json::json!({
                "session_id": "pi-session", "turn_id": "pi-turn",
                "agent_bridge_claim_token": pending.claim_token,
                "agent_bridge_prompt_correlated": correlated,
                "agent_bridge_error": detail,
            });
            if evidence != "missing_token" {
                payload["agent_bridge_cancel_claim_token"] =
                    serde_json::json!(if evidence == "wrong_token" {
                        "1-2-3"
                    } else {
                        &pending.claim_token
                    });
            }
            ADAPTER.handle_hook(directory.path(), &payload).unwrap();
            let paths = event_paths(directory.path()).unwrap();
            if let Some(cancelled) = expected {
                let event: SessionEvent = read_json(&paths[0]).unwrap();
                assert_eq!(event.cancelled, cancelled, "{evidence}");
                let expected_error = if cancelled {
                    format!("cancelled: {detail}")
                } else {
                    detail.to_owned()
                };
                assert_eq!(event.error.as_deref(), Some(expected_error.as_str()));
                assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            } else {
                assert!(paths.is_empty());
                assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            }
        }
    }
}

#[test]
fn pi_hook_owns_the_extension_payload_schema() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let payload = serde_json::json!({
        "session_id": "pi-session",
        "turn_id": "pi-turn",
        "last_assistant_message": marked("pi result", &pending),
        "agent_bridge_claim_token": pending.claim_token,
        "agent_bridge_prompt_correlated": true,
    });

    ADAPTER.handle_hook(directory.path(), &payload).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "pi result");
    assert_eq!(event.provider_session_id.as_deref(), Some("pi-session"));
    assert_eq!(event.turn_id.as_deref(), Some("pi-turn"));
    assert!(directory.path().join(PENDING_TURN_FILE).is_file());
}

#[test]
fn pi_hook_preserves_an_exact_response_when_the_input_was_correlated() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let payload = serde_json::json!({
        "session_id": "pi-session",
        "turn_id": "pi-turn",
        "last_assistant_message": "EXACT_RESPONSE",
        "agent_bridge_claim_token": pending.claim_token,
        "agent_bridge_prompt_correlated": true,
    });

    ADAPTER.handle_hook(directory.path(), &payload).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "EXACT_RESPONSE");
}

#[test]
fn pi_hook_rejects_a_matching_claim_without_input_correlation() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let payload = serde_json::json!({
        "session_id": "pi-session",
        "turn_id": "manual-turn",
        "last_assistant_message": "manual response",
        "agent_bridge_claim_token": pending.claim_token,
    });

    ADAPTER.handle_hook(directory.path(), &payload).unwrap();

    assert!(event_paths(directory.path()).unwrap().is_empty());
    assert!(directory.path().join(TURN_CLAIM_FILE).is_file());
}

#[test]
fn pi_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let initial_pending = claim_pending_turn(directory.path());
    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "session_id": "pi-session",
                "turn_id": "pi-turn-1",
                "last_assistant_message": marked("first result", &initial_pending),
                "agent_bridge_claim_token": initial_pending.claim_token,
                "agent_bridge_prompt_correlated": true,
            }),
        )
        .unwrap();
    let _later_pending = claim_pending_turn(directory.path());
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    ADAPTER
        .handle_hook(
            directory.path(),
            &serde_json::json!({
                "session_id": "pi-session",
                "turn_id": "delayed-old-turn-with-a-new-id",
                "last_assistant_message": "delayed old result",
                "agent_bridge_claim_token": initial_pending.claim_token,
                "agent_bridge_prompt_correlated": true,
            }),
        )
        .unwrap();

    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}
