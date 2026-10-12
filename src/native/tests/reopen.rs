use super::support::*;
use super::*;

fn write_resume_wait_owner(directory: &Path, state: &str, pid: u32) {
    update_status(directory, state.parse().unwrap(), None, None).unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            windows_process_identity: test_windows_process_identity(pid),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

#[test]
fn failed_closed_publication_keeps_legacy_resume_and_claim_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    fs::write(directory.path().join(LEGACY_RESUME_PENDING_FILE), "pending").unwrap();
    fs::write(directory.path().join(LEGACY_RESUME_RUNNING_FILE), "running").unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    fs::remove_file(directory.path().join("status.json")).unwrap();
    fs::create_dir(directory.path().join("status.json")).unwrap();

    assert!(mark_session_closed(directory.path(), None).is_err());

    assert!(directory.path().join(LEGACY_RESUME_PENDING_FILE).exists());
    assert!(directory.path().join(LEGACY_RESUME_RUNNING_FILE).exists());
    assert_eq!(
        fs::read_to_string(directory.path().join(TURN_CLAIM_FILE))
            .unwrap()
            .trim(),
        claim.token()
    );
}

#[test]
fn closed_cleanup_attempts_every_legacy_capability_release() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    fs::create_dir(directory.path().join(LEGACY_RESUME_PENDING_FILE)).unwrap();
    fs::write(directory.path().join(LEGACY_RESUME_RUNNING_FILE), "running").unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();

    assert!(mark_session_closed(directory.path(), None).is_err());

    assert!(!directory.path().join(LEGACY_RESUME_RUNNING_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

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

    assert!(repair_dead_native_owner(directory.path()).unwrap());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}
