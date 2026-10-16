use super::support::*;
use super::*;
use crate::native::session::close as session_close;
#[cfg(any(target_os = "macos", windows))]
use crate::native::terminal::close_authority::{
    TerminalCloseAuthority, test_support::TestObservation,
};

#[test]
fn explicit_close_repairs_failed_launch_without_terminal_record() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("launch failed".to_owned()),
    )
    .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let mut close_was_called = false;

    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| {
            close_was_called = true;
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .unwrap();

    assert!(!close_was_called);
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert!(
        status.error.is_none(),
        "ordinary launch errors are not retained by handle-less close"
    );
}

#[test]
fn explicit_close_remains_available_with_a_corrupt_completion_journal() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    fs::write(directory.path().join(TURN_COMPLETION_FILE), "not-json").unwrap();

    {
        let closer = &mut |_: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
            panic!("a missing terminal handle must not call the adapter")
        };
        session_close::close(
            &Store::open_unchecked(directory.path()),
            None,
            session_close::budget::close_deadline(),
            |surface| closer(surface).map(Into::into),
        )
    }
    .map(|_| ())
    .unwrap();

    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid pending native turn completion"))
    );
}

#[test]
fn already_closed_session_never_reuses_a_stale_terminal_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "stale-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Closed, None, None).unwrap();
    let mut close_calls = 0;

    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| {
            close_calls += 1;
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .unwrap();

    assert_eq!(close_calls, 0);
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state
            .as_str(),
        "closed"
    );
}

#[test]
fn explicit_close_routes_using_the_recorded_terminal_kind() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Ghostty,
            id: "ghostty-terminal".to_owned(),
            tab_id: Some("ghostty-tab".to_owned()),
            window_id: Some("ghostty-window".to_owned()),
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |session| {
            assert_eq!(session.kind, terminal::TerminalKind::Ghostty);
            assert_eq!(session.id, "ghostty-terminal");
            assert_eq!(session.tab_id.as_deref(), Some("ghostty-tab"));
            assert_eq!(session.window_id.as_deref(), Some("ghostty-window"));
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .unwrap();
}

#[test]
fn explicit_close_is_terminal_against_late_native_wrapper_updates() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "closing-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();

    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| Ok(terminal::CloseOutcome::Closed.into()),
    )
    .unwrap();
    update_status(directory.path(), SessionState::Exited, Some(1), None).unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("provider exited after close".to_owned()),
    )
    .unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert_eq!(status.exit_code, None);
    assert_eq!(status.error, None);
}

#[test]
fn internal_session_ids_cannot_escape_the_state_root() {
    assert!(valid_session_id("session-abCD_123-xyz"));
    assert!(!valid_session_id("../outside"));
    assert!(!valid_session_id("session/child"));
}

#[cfg(not(target_os = "macos"))]
fn assert_dead_terminal_owner_close_converges(state: &str) {
    let directory = tempfile::tempdir().unwrap();
    write_owned_terminal_state(directory.path(), state, reaped_child_pid());
    let mut adapter_calls = 0;

    assert!(session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| {
            adapter_calls += 1;
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .unwrap();
    assert!(!session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    session_close::close(
        &Store::open_unchecked(directory.path()),
        None,
        session_close::budget::close_deadline(),
        |_| {
            adapter_calls += 1;
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
    .unwrap();

    assert_eq!(adapter_calls, 0);
    assert!(!directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn exited_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("exited");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn failed_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("failed");
}

#[test]
fn exited_and_failed_live_native_owners_are_not_repaired() {
    for state in ["exited", "failed"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), state, std::process::id());

        assert!(
            !session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap()
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), state);
    }
}

#[test]
fn live_native_session_owner_is_not_repaired() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: std::process::id(),
            windows_process_identity: test_windows_process_identity(std::process::id()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();

    assert!(!session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

#[test]
fn dead_native_session_owner_releases_the_turn_and_closes_state() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no longer running"))
    );
    assert!(directory.path().join("events").is_dir());
}

#[test]
fn terminal_close_transaction_preserves_its_terminal_reason() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();

    session_close::close(
        &Store::open_unchecked(directory.path()),
        Some("native session process stopped".to_owned()),
        crate::native::session::close::budget::close_deadline(),
        |_| panic!("a missing terminal handle must not call the adapter"),
    )
    .unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert_eq!(
        status.error.as_deref(),
        Some("native session process stopped")
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn explicit_close_allows_only_bound_ownerless_launch_failures() {
    let session = terminal::TerminalSession {
        kind: if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::Iterm2
        },
        id: "launch-surface".to_owned(),
        tab_id: None,
        window_id: None,
        managed_session_id: Some("session-owner123".to_owned()),
        wezterm_mux: None,
        windows_process_identity: None,
    };

    let directory = tempfile::tempdir().unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("startup failed".to_owned()),
    )
    .unwrap();
    assert!(
        {
            let deadline = crate::native::session::close::budget::close_deadline();
            TestObservation::on_records(directory.path(), "session-owner123", &session, deadline)
                .decide("session-owner123", &session, deadline)
        }
        .unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );

    let ready_directory = tempfile::tempdir().unwrap();
    update_status(ready_directory.path(), SessionState::Ready, None, None).unwrap();
    assert!(
        {
            let deadline = crate::native::session::close::budget::close_deadline();
            TestObservation::on_records(
                ready_directory.path(),
                "session-owner123",
                &session,
                deadline,
            )
            .decide("session-owner123", &session, deadline)
        }
        .is_err()
    );

    let launching_directory = tempfile::tempdir().unwrap();
    update_status(
        launching_directory.path(),
        SessionState::Launching,
        None,
        None,
    )
    .unwrap();
    assert!(
        {
            let deadline = crate::native::session::close::budget::close_deadline();
            TestObservation::on_records(
                launching_directory.path(),
                "session-different456",
                &session,
                deadline,
            )
            .decide("session-different456", &session, deadline)
        }
        .is_err()
    );
}

#[test]
fn terminal_owner_proof_rejects_record_only_ids_and_reused_surfaces() {
    let terminal: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
        "terminal": "apple-terminal",
        "session_id": "/dev/ttys001",
        "window_id": "1001",
        "managed_session_id": "session-owner123"
    }))
    .unwrap();
    let owner = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: Some("/dev/ttys001".to_owned()),
        terminal_tty_device: Some(7),
        process_start_seconds: Some(100),
        process_start_microseconds: Some(200),
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        terminal_shell: None,
        terminal_app: None,
        windows_process_identity: None,
    };
    let live = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };

    verify_terminal_owner_attestation("session-owner123", &terminal, &owner, &live, 7).unwrap();

    let record_only = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: None,
        terminal_tty_device: None,
        process_start_seconds: None,
        process_start_microseconds: None,
        process_group: None,
        terminal_process_group: None,
        terminal_shell: None,
        terminal_app: None,
        windows_process_identity: None,
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &record_only, &live, 7,)
            .is_err()
    );

    let wrong_session = NativeSessionOwner {
        managed_session_id: Some("session-other456".to_owned()),
        ..owner.clone()
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &wrong_session, &live, 7,)
            .is_err()
    );

    let wrong_tty = NativeSessionOwner {
        terminal_tty: Some("/dev/ttys002".to_owned()),
        ..owner.clone()
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &wrong_tty, &live, 7,)
            .is_err()
    );

    let reused_process = NativeProcessIdentity {
        process_start_microseconds: 201,
        ..live
    };
    assert!(native_owner_identity_matches(&owner, &live));
    assert!(!native_owner_identity_matches(&owner, &reused_process));
    assert!(
        verify_terminal_owner_attestation(
            "session-owner123",
            &terminal,
            &owner,
            &reused_process,
            7,
        )
        .is_err()
    );
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &owner, &live, 8,)
            .is_err()
    );
}

#[test]
fn terminal_owner_process_group_must_match_the_attested_foreground_group() {
    let owner = NativeSessionOwner {
        pid: 4242,
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        ..NativeSessionOwner::default()
    };
    let live = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };

    assert_eq!(
        verified_terminal_owner_process_group(&owner, &live).unwrap(),
        4242
    );

    let changed_foreground_group = NativeProcessIdentity {
        terminal_process_group: 7777,
        ..live
    };
    assert!(verified_terminal_owner_process_group(&owner, &changed_foreground_group).is_err());

    let missing_group = NativeSessionOwner {
        process_group: None,
        terminal_process_group: None,
        ..owner
    };
    assert_eq!(
        verified_terminal_owner_process_group(&missing_group, &live).unwrap(),
        4242
    );

    let partial_group = NativeSessionOwner {
        process_group: Some(4242),
        terminal_process_group: None,
        ..missing_group
    };
    assert!(verified_terminal_owner_process_group(&partial_group, &live).is_err());
}

#[test]
fn terminal_shell_process_group_must_match_the_live_owner_parent_and_tty() {
    let shell = MacTerminalShellIdentity {
        pid: 4000,
        process_group: 4000,
        terminal_tty_device: 7,
        process_start_seconds: 90,
        process_start_microseconds: 100,
    };
    let owner = NativeSessionOwner {
        pid: 4242,
        terminal_shell: Some(shell.clone()),
        ..NativeSessionOwner::default()
    };
    let live_owner = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };
    let live_shell = NativeProcessIdentity {
        pid: 4000,
        parent_pid: 3999,
        terminal_tty_device: 7,
        process_group: 4000,
        terminal_process_group: 4242,
        process_start_seconds: 90,
        process_start_microseconds: 100,
    };

    assert_eq!(
        verified_terminal_shell_process_group(&owner, &live_owner, &live_shell).unwrap(),
        4000
    );

    let wrong_tty = NativeProcessIdentity {
        terminal_tty_device: 8,
        ..live_shell
    };
    assert!(verified_terminal_shell_process_group(&owner, &live_owner, &wrong_tty).is_err());

    let changed_shell = NativeProcessIdentity {
        process_start_microseconds: 101,
        ..live_shell
    };
    assert!(verified_terminal_shell_process_group(&owner, &live_owner, &changed_shell).is_err());
}

#[test]
fn interrupted_close_converges_when_recovery_runs() {
    // A close that stopped after writing the tombstone and status but before releasing the
    // claim and the journal must converge on the next lifecycle-lock holder.
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    let pending = sample_completion(&claim_token, "late result");
    write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
    update_status(
        directory.path(),
        SessionState::Closed,
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
    let tombstone: SessionStatus = read_json(&directory.path().join(CLOSED_STATUS_FILE)).unwrap();

    assert!(recover_pending_completion(directory.path()).unwrap());

    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(event_paths(directory.path()).unwrap().is_empty());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert_eq!(status.generation, tombstone.generation);
    assert_eq!(status.error.as_deref(), Some("closed by the maintainer"));
    let preserved: SessionStatus = read_json(&directory.path().join(CLOSED_STATUS_FILE)).unwrap();
    assert_eq!(preserved.generation, tombstone.generation);
    assert_eq!(preserved.updated_unix_ms, tombstone.updated_unix_ms);
    assert!(!recover_pending_completion(directory.path()).unwrap());
}

#[test]
fn legacy_owner_without_process_identity_is_repaired_only_when_its_pid_is_dead() {
    for (pid, expect_repair) in [(std::process::id(), false), (reaped_child_pid(), true)] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
        claim.retain();
        // Pre-identity owner records carry only a PID on every platform.
        write_json_atomic(
            &directory.path().join(SESSION_OWNER_FILE),
            &serde_json::json!({ "pid": pid }),
        )
        .unwrap();

        assert_eq!(
            session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap(),
            expect_repair
        );
        assert_eq!(
            directory.path().join(TURN_CLAIM_FILE).exists(),
            !expect_repair
        );
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(
            status.state.as_str(),
            if expect_repair { "closed" } else { "working" }
        );
    }
}

#[test]
fn result_wait_repairs_a_dead_owner_and_ends_the_wait() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: reaped_child_pid(),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();

    let started = Instant::now();
    let error = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        None,
        Some(&claim_token),
        agent_bridge::checked_deadline_from(std::time::Instant::now(), Duration::from_secs(30))
            .unwrap(),
        Duration::from_secs(30),
    )
    .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        format!("{error:#}").contains("no longer running"),
        "{error:#}"
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

#[test]
fn a_live_owner_keeps_unrecoverable_journal_damage_as_a_repair_error() {
    // The same damage under a live owner is not repaired away: the error is reported and
    // the claim, the journal, and the status stay exactly as they were.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let pid = std::process::id();
    write_json_atomic(
        &fixture.directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            managed_session_id: Some("session-close123".to_owned()),
            windows_process_identity: test_windows_process_identity(pid),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();

    let error =
        session_close::repair_dead_owner(&Store::open_unchecked(&fixture.directory)).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is missing"),
        "{error:#}"
    );
    let listing = cli_sessions(fixture.root.path());
    assert_eq!(listing[0]["state"], "working", "{listing:?}");
    assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!fixture.directory.join(CLOSED_STATUS_FILE).exists());
    assert!(fixture.directory.join(TERMINAL_HANDLE_FILE).exists());
}

// An explicit close after a failed startup whose wrapper recorded itself and ended.
// A surface with an app-unique native id (an iTerm2 session, a Windows console) is
// still closed by that id. A macOS surface that can outlive its owner is not: a
// failed launch is no close intent, and an owner record without the whole identity
// does not even allow the observation that could prove the surface gone.
// Close authority is terminal-kind policy, so this test lives at the native boundary and
// not in `session::launch`, which must know nothing about terminal kinds.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn explicit_close_recovers_a_failed_startup_only_through_a_stable_native_id() {
    // The launch fixture: a launching session whose claim the launcher retains and whose
    // launch receipt `begin` wrote, as in `session::launch`'s own tests.
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir()
        .unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launch::begin(
        &Store::open_unchecked(directory.path()),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    let id = directory.path().file_name().unwrap().to_str().unwrap();
    let mut record = launch::read(&Reader::open_unchecked(directory.path()))
        .unwrap()
        .unwrap();
    record.phase = launch::Phase::Spawning;
    write_json_atomic(&directory.path().join(launch::FILE), &record).unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("spawn uncertain".to_owned()),
    )
    .unwrap();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: u32::MAX,
            managed_session_id: Some(id.to_owned()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    let authority = |kind, managed_session_id: &str| {
        let deadline = crate::native::session::close::budget::close_deadline();
        let surface = terminal::TerminalSession {
            kind,
            id: "missing-owned-surface".to_owned(),
            managed_session_id: Some(managed_session_id.to_owned()),
            tab_id: None,
            window_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        };
        #[cfg(target_os = "macos")]
        let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
            |_| panic!("no app incarnation is recorded"),
            || panic!("no app incarnation is recorded"),
        );
        let observation = TestObservation::on_records(directory.path(), id, &surface, deadline)
            .with_presence(|| panic!("an unproven owner allows no surface observation"));
        #[cfg(target_os = "macos")]
        let observation =
            observation.with_evidence(if kind == terminal::TerminalKind::AppleTerminal {
                &apple
            } else {
                terminal::close_evidence(kind)
            });
        observation.decide(id, &surface, deadline)
    };
    assert_eq!(
        authority(terminal::TerminalKind::Iterm2, id).unwrap(),
        TerminalCloseAuthority::SurfaceOnly
    );
    assert!(authority(terminal::TerminalKind::Iterm2, "session-foreign").is_err());
    #[cfg(target_os = "macos")]
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
    ] {
        let error = authority(kind, id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no terminal observation or close was sent"),
            "{kind:?}: {error:#}"
        );
    }
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}
