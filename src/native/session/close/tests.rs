use super::*;
use crate::native::session::names::*;
use crate::native::session::primitives::{SyncRecord, read_json, write_json_atomic};
use crate::native::session::schema::SessionEvent;

fn damaged_completion_fixture() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
                cancelled: false,
                provider: "codex".to_owned(),
                message: "unpublished".to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: Some(99),
            },
            status_error: None,
            status_state: SessionState::Ready,
        })
        .unwrap();
    (directory, store)
}

#[test]
fn converge_preserves_damaged_completion_without_a_proven_dead_owner() {
    for pid in [None, Some(std::process::id())] {
        let (_directory, store) = damaged_completion_fixture();
        if let Some(pid) = pid {
            store
                .write_owner(&NativeSessionOwner {
                    pid,
                    ..Default::default()
                })
                .unwrap();
        }
        let before = store.record(CoreRecord::Status).bytes().unwrap();
        assert!(store.converge().is_err());
        assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), before);
        assert!(store.closed_if_present().unwrap().is_none());
        assert!(store.record(CoreRecord::Completion).path().exists());
        assert!(store.record(CoreRecord::TurnClaim).path().exists());
    }
}

#[test]
fn converge_repairs_dead_owner_even_when_completion_recovery_is_damaged() {
    let (_directory, store) = damaged_completion_fixture();
    store
        .write_owner(&NativeSessionOwner {
            pid: 0,
            ..Default::default()
        })
        .unwrap();
    // The missing events directory prevents publication. Repair must still settle
    // the dead owner's session, retaining the damage in its tombstone.
    store.converge().unwrap();
    let closed = store
        .closed_if_present()
        .unwrap()
        .expect("dead owner must be repaired");
    assert_eq!(closed.state, SessionState::Closed);
    assert!(closed.error.unwrap().contains("completion recovery failed"));
    assert!(!store.record(CoreRecord::Completion).path().exists());
    assert!(!store.record(CoreRecord::TurnClaim).path().exists());
    assert!(!store.record(CoreRecord::Events).path().exists());
}

#[test]
fn residual_surface_clears_only_after_the_adapter_confirms_close_or_absence() {
    for typed in [false, true] {
        for outcome in [
            terminal::CloseOutcome::Closed,
            terminal::CloseOutcome::Missing,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open_unchecked(directory.path());
            fs::create_dir(directory.path().join("events")).unwrap();
            update_status_with_residual(
                directory.path(),
                SessionState::Failed,
                None,
                Some(launch::RESIDUAL_SURFACE_MARKER.to_owned()),
                typed.then_some(launch::ResidualSurface::Unverified),
            )
            .unwrap();
            store
                .write_terminal(&terminal::TerminalSession {
                    kind: terminal::TerminalKind::Iterm2,
                    id: "exact-retained-surface".to_owned(),
                    tab_id: None,
                    window_id: None,
                    managed_session_id: None,
                    wezterm_mux: None,
                    windows_process_identity: None,
                })
                .unwrap();
            let refusal = close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |surface| {
                    assert_eq!(surface.id, "exact-retained-surface");
                    bail!("close authority not established")
                },
            );
            assert!(refusal.is_err());
            assert_eq!(
                store.status().unwrap().residual_surface(),
                Some(launch::ResidualSurface::Unverified)
            );
            assert!(store.record(CoreRecord::Terminal).path().exists());
            // A normal status change cannot erase the typed or migrated legacy fact.
            update_status(directory.path(), SessionState::Failed, None, None).unwrap();
            assert_eq!(
                store.status().unwrap().residual_surface(),
                Some(launch::ResidualSurface::Unverified)
            );
            close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |_| Ok(outcome.into()),
            )
            .unwrap();
            let closed = store.status().unwrap();
            assert_eq!(
                closed.residual_surface,
                Some(launch::ResidualSurface::Cleared)
            );
            assert_eq!(closed.residual_surface(), None);
            store.converge().unwrap();
            close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |_| panic!("consumed handle must not close again"),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(store.status().unwrap()).unwrap(),
                serde_json::to_value(&closed).unwrap()
            );
            assert_eq!(
                serde_json::to_value(store.closed_if_present().unwrap().unwrap()).unwrap(),
                serde_json::to_value(closed).unwrap()
            );
        }
    }
}

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
                residual_surface: None,
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
                    cancelled: false,
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
            close(
                &store,
                None,
                crate::native::session::close::budget::close_deadline(),
                |session| {
                    assert_eq!(session.kind, terminal::TerminalKind::Iterm2);
                    assert_eq!(session.id, "missing-iterm-session");
                    close_calls += 1;
                    Ok(terminal::CloseOutcome::Missing.into())
                },
            )
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

    close(
        &Store::open_unchecked(directory.path()),
        None,
        crate::native::session::close::budget::close_deadline(),
        |session| {
            calls += 1;
            assert_eq!(session.id, "interrupted-close");
            Ok(terminal::CloseOutcome::Closed.into())
        },
    )
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
            residual_surface: None,
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

#[cfg(windows)]
#[test]
fn close_deadline_converge_keeps_expired_callers_records() {
    let (_directory, store) = damaged_completion_fixture();
    store
        .write_owner(&NativeSessionOwner {
            pid: 0,
            ..Default::default()
        })
        .unwrap();
    let status = store.record(CoreRecord::Status).bytes().unwrap();
    let error = store.converge_until(Instant::now()).unwrap_err();
    assert!(format!("{error:#}").contains("deadline"));
    assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), status);
    assert!(store.record(CoreRecord::Completion).path().exists());
    assert!(store.record(CoreRecord::TurnClaim).path().exists());
}

#[cfg(windows)]
#[test]
fn close_deadline_repair_after_slow_owner_observation_does_not_settle() {
    let (_directory, store) = damaged_completion_fixture();
    store
        .write_owner(&NativeSessionOwner {
            pid: 0,
            ..Default::default()
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    let result = repair_dead_owner_with(
        &store,
        |_| {
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Ok(false)
        },
        |_| false,
        |_| panic!("expired repair must not close"),
        deadline,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
    assert!(store.closed_if_present().unwrap().is_none());
}

#[test]
fn close_deadline_preclose_repair_does_not_get_another_budget() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let deadline = Instant::now() + Duration::from_millis(10);
    let result = close_with_repair(
        &store,
        None,
        || {
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Ok(false)
        },
        |_| panic!("repair spent the close deadline"),
        deadline,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

fn deadline_fixture() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    store
        .write_terminal(&terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "deadline-fixture".into(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        })
        .unwrap();
    (directory, store)
}

// Freeze time while recovery and syncs run; expire only after the lock actually
// refuses a non-blocking attempt. The parent keeps the lock until this reports.
fn expire_at_contended_lock<T>(name: &'static str, run: impl FnOnce(Instant) -> T) -> T {
    use super::super::primitives::close_io_test::{self, Point};
    let now = Instant::now();
    let clock = crate::native::session::close::budget::TestCloseClock::new(now);
    let deadline = now + Duration::from_secs(1);
    let observed = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = observed.clone();
    let result = close_io_test::during(
        move |point| {
            if let Point::WouldBlock(actual) = point {
                assert_eq!(actual, name);
                seen.set(true);
                clock.set(deadline);
            }
        },
        || run(deadline),
    );
    assert!(observed.get(), "the test must reach the contended lock");
    result
}

#[test]
fn close_deadline_bounds_both_lifecycle_lock_acquisitions_without_changes() {
    for recovery in [true, false] {
        let (_directory, store) = deadline_fixture();
        let held = store.lock().unwrap();
        let before = store.record(CoreRecord::Status).bytes().unwrap();
        let terminal = store.record(CoreRecord::Terminal).bytes().unwrap();
        let directory = store.directory().to_owned();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let store = Store::open_unchecked(directory);
            let result = expire_at_contended_lock("turn claim lifecycle", |deadline| {
                if recovery {
                    close(&store, None, deadline, |_| {
                        panic!("locked close must not act")
                    })
                    .map(|_| ())
                } else {
                    close_session_state_with_error(
                        &store,
                        None,
                        |_| panic!("locked close must not act"),
                        deadline,
                    )
                }
            });
            send.send(result.map_err(|e| format!("{e:#}"))).unwrap();
        });
        // Keep the lock until the worker reports. The watchdog only prevents a broken
        // implementation from hanging the suite; it does not release the lock on time.
        let result = receive.recv_timeout(Duration::from_secs(10));
        assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), before);
        assert_eq!(
            store.record(CoreRecord::Terminal).bytes().unwrap(),
            terminal
        );
        assert!(!store.record(CoreRecord::TerminalClosing).path().exists());
        drop(held);
        worker.join().unwrap();
        let error = result
            .expect("close waited past its deadline for the lifecycle lock")
            .unwrap_err();
        assert!(error.contains("another close or turn"), "{error}");
        assert!(error.contains("nothing was changed"), "{error}");
        close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |_| Ok(terminal::CloseOutcome::Closed.into()),
        )
        .unwrap();
    }
}

#[test]
fn close_deadline_status_lock_retains_consumption_for_next_close() {
    let (_directory, store) = deadline_fixture();
    let held = store.lock_status().unwrap();
    let before = store.record(CoreRecord::Status).bytes().unwrap();
    let directory = store.directory().to_owned();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let store = Store::open_unchecked(directory);
        let result = expire_at_contended_lock("status", |deadline| {
            close(&store, None, deadline, |_| {
                Ok(terminal::CloseOutcome::Closed.into())
            })
        });
        send.send(result.map_err(|e| format!("{e:#}"))).unwrap();
    });
    let result = receive.recv_timeout(Duration::from_secs(10));
    assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), before);
    assert!(store.record(CoreRecord::TerminalClosed).path().exists());
    drop(held);
    worker.join().unwrap();
    let error = result
        .expect("settlement waited past its deadline for the status lock")
        .unwrap_err();
    assert!(error.contains("another close or turn"), "{error}");
    assert!(store.closed_if_present().unwrap().is_none());
    close(
        &store,
        None,
        crate::native::session::close::budget::close_deadline(),
        |_| panic!("consumed handle must not act again"),
    )
    .unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
}

#[cfg(target_os = "macos")]
#[test]
fn close_deadline_expiry_after_durable_markers_never_rolls_back() {
    for point in [
        interruption::Point::ConsumptionMarkerWritten,
        interruption::Point::ClosedTombstoneWritten,
    ] {
        let (_directory, store) = deadline_fixture();
        store
            .record(CoreRecord::TurnClaim)
            .write_private(b"1-2-3\n")
            .unwrap();
        let now = Instant::now();
        let clock = crate::native::session::close::budget::TestCloseClock::new(now);
        let deadline = now + Duration::from_secs(1);
        let result = interruption::expiring(point, deadline, || {
            close(&store, None, deadline, |_| {
                Ok(terminal::CloseOutcome::Closed.into())
            })
        });
        assert!(
            result.is_err(),
            "expiry after {point:?} must report incomplete settlement"
        );
        assert!(store.record(CoreRecord::TerminalClosed).path().exists());
        assert!(
            !store.record(CoreRecord::Terminal).path().exists(),
            "consumed handle cannot be restored"
        );
        assert!(store.record(CoreRecord::TurnClaim).path().exists());
        let tombstone = store.record(CoreRecord::Closed).bytes().unwrap();
        assert_eq!(
            tombstone.is_some(),
            point == interruption::Point::ClosedTombstoneWritten
        );
        clock.set(now);
        close(&store, None, deadline, |_| {
            panic!("committed close must not call the adapter")
        })
        .unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert!(!store.has_active_session_capability());
        if let Some(tombstone) = tombstone {
            assert_eq!(
                store.record(CoreRecord::Closed).bytes().unwrap().unwrap(),
                tombstone
            );
        }
        store.converge().unwrap();
    }
}

#[test]
fn close_deadline_contender_does_not_disturb_the_closer_holding_the_lock() {
    let (_directory, store) = deadline_fixture();
    let path = store.directory().to_owned();
    let (entered, entry) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        close(
            &Store::open_unchecked(path),
            None,
            crate::native::session::close::budget::close_deadline(),
            |_| {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(terminal::CloseOutcome::Closed.into())
            },
        )
    });
    entry.recv_timeout(Duration::from_secs(10)).unwrap();
    let status = store.record(CoreRecord::Status).bytes().unwrap();
    let claimed = store.record(CoreRecord::TerminalClosing).bytes().unwrap();
    let result = expire_at_contended_lock("turn claim lifecycle", |deadline| {
        close(&store, None, deadline, |_| panic!("contender cannot close"))
    });
    assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), status);
    assert_eq!(
        store.record(CoreRecord::TerminalClosing).bytes().unwrap(),
        claimed
    );
    release.send(()).unwrap();
    assert!(format!("{:#}", result.unwrap_err()).contains("another close or turn"));
    holder.join().unwrap().unwrap();
    assert!(status.is_some() && claimed.is_some());
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
}

#[cfg(windows)]
#[test]
fn close_deadline_windows_record_replacement_stops_at_close_budget() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.json");
    write_json_atomic(&path, &"before").unwrap();
    // Deliberately deny delete sharing, making replacement fail until we drop this.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .unwrap();
    use super::super::primitives::close_io_test::{self, Point};
    let now = Instant::now();
    let clock = crate::native::session::close::budget::TestCloseClock::new(now);
    let deadline = now + Duration::from_millis(40);
    let retries = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = retries.clone();
    let result = {
        let _budget = budget::Scope::enter(deadline);
        close_io_test::during(
            move |point| {
                if point == Point::RecordReplaceRetry {
                    seen.set(seen.get() + 1);
                    // Prove that a failed replacement was retried, then exhaust the
                    // close budget while its ordinary one-second budget remains.
                    assert!(seen.get() <= 2, "replacement retried after close expiry");
                    if seen.get() == 2 {
                        clock.set(deadline);
                    }
                }
            },
            || write_json_atomic(&path, &"after"),
        )
    };
    assert_eq!(retries.get(), 2);
    let error = result.unwrap_err();
    assert!(
        format!("{error:#}").contains("close deadline expired"),
        "{error:#}"
    );
    assert_eq!(read_json::<String>(&path).unwrap(), "before");
    drop(held);
    write_json_atomic(&path, &"after").unwrap();
    assert_eq!(read_json::<String>(&path).unwrap(), "after");
}

#[cfg(windows)]
#[test]
fn close_deadline_public_repair_of_exited_or_failed_windows_session_needs_no_helper() {
    for state in [SessionState::Exited, SessionState::Failed] {
        let (_directory, store) = deadline_fixture();
        // These are the durable records after a helper ended some console processes.
        update_status(store.directory(), state, None, None).unwrap();
        store
            .write_owner(&NativeSessionOwner {
                pid: 0,
                ..Default::default()
            })
            .unwrap();
        store
            .record(CoreRecord::Terminal)
            .rename_to(&store.record(CoreRecord::TerminalClosing))
            .unwrap();
        store
            .record(CoreRecord::TurnClaim)
            .write_private(b"1-2-3\n")
            .unwrap();
        // Iterm2 is intentionally unsupported on Windows: any helper dispatch fails.
        // Public convergence must settle these states from records alone.
        store.converge().unwrap();
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert!(!store.has_active_session_capability());
        assert!(store.record(CoreRecord::TerminalClosed).path().exists());
        store.converge().unwrap();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn close_deadline_before_confirmation_keeps_handle_and_intent() {
    let (_directory, store) = deadline_fixture();
    let now = Instant::now();
    let clock = crate::native::session::close::budget::TestCloseClock::new(now);
    let deadline = now + Duration::from_secs(1);
    let handle = store.record(CoreRecord::Terminal).bytes().unwrap();
    let error = close(&store, None, deadline, |_| {
        store
            .record(CoreRecord::TerminalCloseIntent)
            .write_json(&serde_json::json!({"intent": "retained"}))?;
        clock.set(deadline);
        Ok(terminal::CloseOutcome::Closed.into())
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("closing was not confirmed"));
    assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), handle);
    assert!(
        store
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists()
    );
    assert!(!store.record(CoreRecord::TerminalClosed).path().exists());
    assert!(store.closed_if_present().unwrap().is_none());
    clock.set(now);
    let retry = close(&store, None, deadline, |_| {
        anyhow::bail!("fresh authority refused")
    });
    assert!(
        retry
            .unwrap_err()
            .to_string()
            .contains("fresh authority refused")
    );
    assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), handle);
    assert!(
        store
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists()
    );
}

#[test]
fn close_deadline_scope_is_restored_and_other_threads_keep_blocking_locks() {
    let (_directory, store) = deadline_fixture();
    let held = store.lock().unwrap();
    let path = store.directory().to_owned();
    use super::super::primitives::close_io_test::{self, Point};
    let (send, receive) = std::sync::mpsc::channel();
    let (entered, entry) = std::sync::mpsc::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    {
        let expired = Instant::now();
        let _scope = budget::Scope::enter(expired);
        {
            let _nested = budget::Scope::enter(expired + Duration::from_secs(60));
            assert_eq!(budget::deadline(), Some(expired));
        }
        assert_eq!(budget::deadline(), Some(expired));
        let worker = std::thread::spawn(move || {
            assert!(budget::deadline().is_none());
            close_io_test::during(
                move |point| {
                    if point == Point::BlockingLock {
                        entered.send(()).unwrap();
                        resumed.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                },
                || {
                    let _lock = Store::open_unchecked(path).lock().unwrap();
                },
            );
            send.send(()).unwrap();
        });
        entry.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(
            receive.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(held);
        resume.send(()).unwrap();
        receive.recv_timeout(Duration::from_secs(10)).unwrap();
        worker.join().unwrap();
    }
    assert!(budget::deadline().is_none());
    let _lock = store.lock().unwrap();
}

#[test]
fn close_deadline_uncontended_settlement_reports_only_expiry() {
    use super::super::primitives::close_io_test::{self, Point};
    let (_directory, store) = deadline_fixture();
    let now = Instant::now();
    let clock = crate::native::session::close::budget::TestCloseClock::new(now);
    let deadline = now + Duration::from_secs(1);
    let result = close_io_test::during(
        move |point| {
            if point == Point::BeforeLock("status") {
                crate::native::session::close::budget::CLOSE_TEST_TIME
                    .with(|time| time.set(Some(deadline)));
            }
            assert!(!matches!(point, Point::WouldBlock(_)), "no lock is held");
        },
        || {
            close(&store, None, deadline, |_| {
                Ok(terminal::CloseOutcome::Closed.into())
            })
        },
    );
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("close deadline expired"), "{error}");
    assert_eq!(
        error.matches("close deadline expired").count(),
        1,
        "{error}"
    );
    assert!(!error.contains("another close or turn"), "{error}");
    assert!(store.record(CoreRecord::TerminalClosed).path().exists());
    assert!(store.closed_if_present().unwrap().is_none());
    clock.set(now);
    close(&store, None, deadline, |_| {
        panic!("consumed handle must not close again")
    })
    .unwrap();
}
