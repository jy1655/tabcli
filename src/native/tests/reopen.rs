use super::*;
use crate::native::session::close as session_close;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::{SESSION_OWNER_FILE, TURN_CLAIM_FILE};
use crate::native::session::{SessionState, SessionStatus, update_status};
use crate::native::terminal::ownership::NativeSessionOwner;
use std::fs;

#[test]
fn resume_pending_dead_owner_is_repaired_instead_of_stalling() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::ResumePending, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: reaped_child_pid(),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();

    assert!(session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}
