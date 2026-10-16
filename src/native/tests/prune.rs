use super::support::*;
use super::*;
use crate::native::prune::prune_closed_sessions;

#[test]
fn pruning_removes_only_old_quiescent_closed_session_directories() {
    let root = tempfile::tempdir().unwrap();
    let old = write_prune_test_session(root.path(), "session-old123", "closed", Some(100));
    let recent = write_prune_test_session(root.path(), "session-recent123", "closed", Some(900));
    let live = write_prune_test_session(root.path(), "session-live123", "ready", None);
    let claimed = write_prune_test_session(root.path(), "session-claimed123", "closed", Some(100));
    fs::write(claimed.join(TURN_CLAIM_FILE), "still-owned").unwrap();
    #[cfg(not(windows))]
    let owned = {
        let owned = write_prune_test_session(root.path(), "session-owned123", "closed", Some(100));
        write_json_atomic(
            &owned.join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: std::process::id(),
                managed_session_id: Some("session-owned123".to_owned()),
                ..NativeSessionOwner::default()
            },
        )
        .unwrap();
        owned
    };
    #[cfg(windows)]
    let owned = {
        let owned = write_prune_test_session(root.path(), "session-owned123", "closed", Some(100));
        write_json_atomic(
            &owned.join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: std::process::id(),
                managed_session_id: Some("session-owned123".to_owned()),
                windows_process_identity: Some(
                    terminal::windows_process_identity(std::process::id()).unwrap(),
                ),
                ..NativeSessionOwner::default()
            },
        )
        .unwrap();
        owned
    };

    let removed = prune_closed_sessions(root.path(), 500).unwrap();

    assert_eq!(removed, ["session-old123"]);
    assert!(!old.exists());
    assert!(recent.exists());
    assert!(live.exists());
    assert!(claimed.exists());
    assert!(owned.exists());
}

#[test]
fn pruning_skips_malformed_closed_records_without_blocking_valid_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let valid = write_prune_test_session(root.path(), "session-valid123", "closed", Some(100));
    let malformed = write_prune_test_session(root.path(), "session-broken123", "closed", Some(100));
    fs::write(malformed.join(CLOSED_STATUS_FILE), "not-json").unwrap();

    let removed = prune_closed_sessions(root.path(), 500).unwrap();

    assert_eq!(removed, ["session-valid123"]);
    assert!(!valid.exists());
    assert!(malformed.exists());
}

#[cfg(unix)]
#[test]
fn pruning_never_follows_a_session_directory_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = write_prune_test_session(outside.path(), "session-target123", "closed", Some(100));
    symlink(&target, root.path().join("session-link123")).unwrap();

    assert!(prune_closed_sessions(root.path(), 500).unwrap().is_empty());
    assert!(target.exists());
}
