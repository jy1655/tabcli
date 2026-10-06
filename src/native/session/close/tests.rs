use super::*;

#[test]
fn converge_publishes_completion_before_repairing_dead_owner() {
    super::super::tests::with_fixed_time(|| {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        fs::create_dir(store.record(CoreRecord::Events).path()).unwrap();
        store
            .write_status(&SessionStatus {
                state: SessionState::Working,
                generation: 7,
                updated_unix_ms: 100,
                exit_code: None,
                error: None,
            })
            .unwrap();
        store
            .write_owner(&NativeSessionOwner {
                pid: 0,
                ..Default::default()
            })
            .unwrap();
        store
            .record(CoreRecord::TurnClaim)
            .write_private(b"1-2-3\n")
            .unwrap();
        store
            .write_completion(&PendingTurnCompletion {
                schema: 1,
                claim_token: "1-2-3".to_owned(),
                event_file: "event-fixture.json".to_owned(),
                event: SessionEvent {
                    provider: "codex".to_owned(),
                    message: "completed before owner exit".to_owned(),
                    error: None,
                    provider_session_id: Some("provider-session".to_owned()),
                    turn_id: Some("provider-turn".to_owned()),
                    created_unix_ms: Some(99),
                },
                status_error: None,
                status_state: SessionState::Ready,
            })
            .unwrap();

        store.converge().unwrap();

        // Captured by running the old recover-then-repair pair before introducing converge.
        for (name, expected) in [
            (
                "status.json",
                include_bytes!("../fixtures/converge/status.json").as_slice(),
            ),
            (
                "closed.json",
                include_bytes!("../fixtures/converge/closed.json").as_slice(),
            ),
            (
                "events/event-fixture.json",
                include_bytes!("../fixtures/converge/events/event-fixture.json").as_slice(),
            ),
        ] {
            assert_eq!(
                fs::read(directory.path().join(name)).unwrap(),
                expected,
                "{name}"
            );
        }
        assert_eq!(store.events().unwrap().len(), 1);
        assert!(!store.record(CoreRecord::TurnClaim).path().exists());
        assert!(!store.record(CoreRecord::Completion).path().exists());
    });
}

#[test]
fn explicit_close_consumes_the_handle_and_repeated_close_skips_the_adapter() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "missing-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();
    let store = Store::open_unchecked(directory.path());
    let mut tombstone = None;
    let mut close_calls = 0;

    for attempt in 0..2 {
        let (outcome, syncs) = with_sync_log(|| {
            close(&store, None, |session| {
                assert_eq!(session.kind, terminal::TerminalKind::Iterm2);
                assert_eq!(session.id, "missing-iterm-session");
                close_calls += 1;
                Ok(terminal::CloseOutcome::Missing)
            })
        });
        assert_eq!(outcome.unwrap(), terminal::CloseOutcome::Missing);
        let closed = store.record(CoreRecord::Closed);
        assert_eq!(
            syncs
                .iter()
                .filter(|entry| {
                    matches!(entry, SyncRecord::File(path) if path == closed.path())
                })
                .count(),
            usize::from(attempt == 0),
            "closed.json is written only once"
        );
        let bytes = store.record(CoreRecord::Closed).bytes().unwrap().unwrap();
        if let Some(previous) = &tombstone {
            assert_eq!(&bytes, previous, "a second close preserves the tombstone");
        }
        tombstone = Some(bytes);
    }

    assert_eq!(close_calls, 1);
    assert!(!directory.path().join("terminal.json").exists());
    assert!(directory.path().join("terminal.closed.json").exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

#[test]
fn interrupted_terminal_close_resumes_from_the_claimed_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_CLOSING_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "interrupted-close".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut calls = 0;

    close(&Store::open_unchecked(directory.path()), None, |session| {
        calls += 1;
        assert_eq!(session.id, "interrupted-close");
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();

    assert_eq!(calls, 1);
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state
            .as_str(),
        "closed"
    );
}

#[test]
fn interrupted_close_without_tombstone_converges_after_owner_exit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    store
        .write_status(&SessionStatus {
            state: SessionState::Exited,
            generation: 2,
            updated_unix_ms: 100,
            exit_code: Some(0),
            error: None,
        })
        .unwrap();
    store
        .write_owner(&NativeSessionOwner {
            pid: 0,
            ..Default::default()
        })
        .unwrap();
    store
        .record(CoreRecord::TerminalClosing)
        .write_json(&terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "interrupted-close".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        })
        .unwrap();
    for record in [
        CoreRecord::TurnClaim,
        CoreRecord::LegacyResumePending,
        CoreRecord::LegacyResumeRunning,
    ] {
        store.record(record).write_private(b"1-2-3\n").unwrap();
    }
    assert!(store.closed_if_present().unwrap().is_none());
    store.converge().unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
    let tombstone = store.record(CoreRecord::Closed).bytes().unwrap().unwrap();
    assert_eq!(
        store.record(CoreRecord::Status).bytes().unwrap().unwrap(),
        tombstone
    );
    assert!(store.record(CoreRecord::TerminalClosed).path().exists());
    assert!(!store.has_active_session_capability());
    store.converge().unwrap();
    assert_eq!(
        store.record(CoreRecord::Closed).bytes().unwrap().unwrap(),
        tombstone
    );
}

#[test]
fn reader_reports_capabilities_and_owner_evidence_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let reader = Reader::open_unchecked(directory.path());
    assert!(!reader.has_active_session_capability());
    assert!(!reader.native_owner_blocks_prune().unwrap());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    for record in [
        CoreRecord::Terminal,
        CoreRecord::TerminalClosing,
        CoreRecord::TurnClaim,
        CoreRecord::Completion,
        CoreRecord::LegacyResumePending,
        CoreRecord::LegacyResumeRunning,
    ] {
        store.record(record).write_private(b"retained").unwrap();
        assert!(reader.has_active_session_capability());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        store.record(record).remove().unwrap();
        assert!(!reader.has_active_session_capability());
    }
    store
        .record(CoreRecord::Owner)
        .write_private(b"invalid owner")
        .unwrap();
    assert!(reader.native_owner_blocks_prune().unwrap());
    assert_eq!(
        store.record(CoreRecord::Owner).bytes().unwrap().unwrap(),
        b"invalid owner"
    );
    store
        .write_owner(&NativeSessionOwner {
            pid: std::process::id(),
            ..Default::default()
        })
        .unwrap();
    assert!(reader.native_owner_blocks_prune().unwrap());
    store
        .write_owner(&NativeSessionOwner {
            pid: 0,
            ..Default::default()
        })
        .unwrap();
    // Windows requires attested identity even for a dead PID, exactly as before.
    assert_eq!(reader.native_owner_blocks_prune().unwrap(), cfg!(windows));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}
