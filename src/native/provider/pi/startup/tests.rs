use super::super::test_support::claim_pending_turn;
use super::*;
use crate::native::{TURN_CLAIM_FILE, write_json_atomic};
use std::fs;

#[test]
fn unresolved_project_trust_never_receives_the_initial_console_paste() {
    let directory = tempfile::tempdir().unwrap();
    claim_pending_turn(directory.path());
    let sent = std::cell::Cell::new(false);
    let result = send_initial_prompt_after_startup(directory.path(), Instant::now(), || {
        sent.set(true);
        Ok(())
    });
    assert!(
        !sent.get(),
        "initial input could press Enter on the unresolved trust dialog"
    );
    let failure = result.unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
}

#[test]
fn startup_receipt_allows_one_paste_and_preserves_the_transport_outcome() {
    for schema in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let pending = claim_pending_turn(directory.path());
        write_json_atomic(
            &directory.path().join(STARTUP_READY_FILE),
            &StartupReady {
                schema,
                claim_token: pending.claim_token,
                session_id: "pi-native-session".to_owned(),
            },
        )
        .unwrap();
        let sends = std::cell::Cell::new(0);
        let failure = send_initial_prompt_after_startup(
            directory.path(),
            Instant::now() + Duration::from_secs(1),
            || {
                sends.set(sends.get() + 1);
                Err(terminal::TerminalSendFailure::delivery_uncertain(
                    anyhow::anyhow!("transport uncertain"),
                ))
            },
        )
        .unwrap_err();
        assert_eq!(sends.get(), 1);
        assert!(failure.delivery_may_have_occurred());
        assert!(
            failure
                .into_error()
                .to_string()
                .contains("transport uncertain")
        );
    }
}

#[test]
fn stale_malformed_or_cancelled_startup_never_pastes() {
    let directory = tempfile::tempdir().unwrap();
    let pending = claim_pending_turn(directory.path());
    let valid = serde_json::json!({"schema": 1, "claim_token": pending.claim_token, "session_id": "native"});
    for receipt in [
        "not-json".to_owned(),
        serde_json::json!({"schema": 1, "claim_token": "1-2-3", "session_id": "native"}).to_string(),
        serde_json::json!({"schema": 3, "claim_token": pending.claim_token, "session_id": "native"}).to_string(),
        serde_json::json!({"schema": 1, "claim_token": pending.claim_token, "session_id": ""}).to_string(),
    ] {
        fs::write(directory.path().join(STARTUP_READY_FILE), receipt).unwrap();
        let failure = send_initial_prompt_after_startup(directory.path(), Instant::now() + Duration::from_secs(1), || panic!("unexpected paste")).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
    }
    fs::write(directory.path().join(STARTUP_READY_FILE), valid.to_string()).unwrap();
    fs::remove_file(directory.path().join(TURN_CLAIM_FILE)).unwrap();
    let failure = send_initial_prompt_after_startup(
        directory.path(),
        Instant::now() + Duration::from_secs(1),
        || panic!("cancelled turn pasted"),
    )
    .unwrap_err();
    assert!(!failure.delivery_may_have_occurred());
}
