use super::super::test_support::claim_pending_turn;
use super::*;
use crate::native::session::SessionState;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::TURN_CLAIM_FILE;
use crate::native::session::{SessionStatus, update_status};
use std::fs;

#[test]
fn hook_transport_failure_signal_recovers_the_bridge_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    write_json_atomic(
        &directory.path().join(HOOK_FAILURE_FILE),
        &HookFailureSignal {
            error: "native hook exited with status 1".to_owned(),
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            claim_token: Some(pending.claim_token),
            usage: Some(serde_json::json!({"coverage": "uncertain", "runs": 2,
                "assistant_messages": 3, "input_tokens": 11, "output_tokens": 7,
                "cache_read_tokens": 0, "cache_write_tokens": 0, "total_tokens": 18})),
            usage_error: None,
        },
    )
    .unwrap();

    assert!(consume_hook_failure(directory.path()).unwrap());
    // The failure record keeps what Pi reported before delivery failed.
    let events = crate::native::session::primitives::event_paths(directory.path()).unwrap();
    let event: crate::native::session::schema::SessionEvent = read_json(&events[0]).unwrap();
    let usage = event.usage.unwrap();
    assert_eq!(
        (usage.runs, usage.total_tokens, usage.coverage.as_str()),
        (2, 18, "uncertain")
    );
    assert_eq!(event.usage_error, None);
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(HOOK_FAILURE_FILE).exists());
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
    assert!(format!("{error:#}").contains("native hook exited with status 1"));
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
}

// A malformed usage field is bounded after the claim is checked, never before: the signal
// is parsed with both usage fields raw, so a stale signal with a non-string `usage_error`
// is ignored like any stale signal, and a current one records the bounded error.
#[test]
fn non_string_usage_error_is_bounded_after_the_claim_is_checked() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(directory.path());
    let signal = directory.path().join(HOOK_FAILURE_FILE);

    // Stale claim: consumed and ignored; the monitor goes on, the pending turn is untouched.
    fs::write(
        &signal,
        r#"{"error":"native hook exited with status 1","claim_token":"0-0-0","usage_error":5}"#,
    )
    .unwrap();
    assert!(consume_hook_failure(directory.path()).unwrap());
    assert!(!signal.exists());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(
        crate::native::session::primitives::event_paths(directory.path())
            .unwrap()
            .is_empty()
    );
    assert!(!consume_hook_failure(directory.path()).unwrap());

    // Current claim: the correlated failure is recorded with the bounded usage error.
    fs::write(
        &signal,
        format!(
            r#"{{"error":"native hook exited with status 1","claim_token":"{}","usage_error":5}}"#,
            pending.claim_token
        ),
    )
    .unwrap();
    assert!(consume_hook_failure(directory.path()).unwrap());
    let events = crate::native::session::primitives::event_paths(directory.path()).unwrap();
    let event: crate::native::session::schema::SessionEvent = read_json(&events[0]).unwrap();
    assert_eq!(event.usage, None);
    assert_eq!(event.usage_error.as_deref(), Some("malformed usage_error"));
    assert!(
        event
            .error
            .as_deref()
            .unwrap()
            .contains("native hook exited with status 1")
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}
