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
        },
    )
    .unwrap();

    assert!(consume_hook_failure(directory.path()).unwrap());
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
