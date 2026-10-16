use super::*;
#[cfg(target_os = "macos")]
use crate::native::MacTerminalAppIdentity;
use crate::native::prune::prune_closed_sessions;
use crate::native::query::observation::{Observation, ResultReference};
use crate::native::session::close as session_close;
use crate::native::session::names::*;
use crate::native::session::primitives::event_paths;
use crate::native::session::primitives::{SyncRecord, read_json, write_json_atomic};
use crate::native::session::schema::SessionEvent;
use crate::native::session::schema::{EVENT_READ_LIMIT, SESSION_SCHEMA, SessionManifest};
use crate::native::session::turn::PendingTurnCompletion;
use crate::native::session::{Reader, Store, turn};
use crate::native::sessions::sessions_in;
#[cfg(target_os = "macos")]
use crate::native::terminal::close_authority::{
    TerminalCloseAuthority, test_support::TestObservation,
};
use crate::native::tests::test_windows_process_identity;
#[cfg(target_os = "macos")]
use crate::native::tests::write_attested_apple_terminal_state;
use crate::native::{
    NativeCommand, context, doctor, injected_fault, parse_args, query, with_fault_budget,
};
use agent_bridge::FirstPartyCli;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command};
use std::{sync::atomic::Ordering, thread};

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
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
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
    // A legacy dead PID blocks pruning on no platform.
    assert!(!reader.native_owner_blocks_prune().unwrap());
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

#[test]
fn attestation_unknown_preserves_completion_damage_and_claim_without_closing() {
    let (_directory, store) = damaged_completion_fixture();
    store
        .write_owner(&NativeSessionOwner {
            pid: 4242,
            ..Default::default()
        })
        .unwrap();
    let before = store.record(CoreRecord::Status).bytes().unwrap();
    let observed = std::cell::Cell::new(false);
    let error = repair_dead_owner_with(
        &store,
        |_| {
            observed.set(true);
            // Recovery has already tried publication and retained the damaged journal.
            assert!(store.record(CoreRecord::Completion).path().exists());
            repair_owner_is_live(&OwnerAttestation {
                process: ProcessObservation::Unknown("inspection denied".into()),
                control: super::super::owner::ControlAttestation::NotRecorded,
                pid_alive: Some(true),
            })
        },
        |_| false,
        |_| panic!("unknown grants no close authority"),
        budget::close_deadline(),
    )
    .unwrap_err();
    assert!(observed.get());
    assert!(error.to_string().contains("inspection denied"));
    assert_eq!(store.record(CoreRecord::Status).bytes().unwrap(), before);
    assert!(store.record(CoreRecord::Completion).path().exists());
    assert!(store.record(CoreRecord::TurnClaim).path().exists());
    assert!(store.closed_if_present().unwrap().is_none());
}

#[test]
fn attestation_policy_matrix_keeps_live_and_unknown_owners() {
    use crate::native::process_identity::{Attested, Gone};
    for (process, blocked, live) in [
        (
            ProcessObservation::Live(Attested::Incarnation),
            true,
            Some(true),
        ),
        (
            ProcessObservation::Live(Attested::PidOnly),
            true,
            Some(true),
        ),
        (ProcessObservation::Gone(Gone::Exited), false, Some(false)),
        (
            ProcessObservation::Gone(Gone::Reused("birth".into())),
            false,
            Some(false),
        ),
        (ProcessObservation::Unknown("denied".into()), true, None),
    ] {
        let a = OwnerAttestation {
            process,
            control: super::super::owner::ControlAttestation::NotRecorded,
            pid_alive: None,
        };
        assert_eq!(prune_blocked_by(&a), blocked);
        assert_eq!(repair_owner_is_live(&a).ok(), live);
    }
}

#[test]
fn explicit_close_restores_the_terminal_handle_after_adapter_failure() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::WindowsConsole,
            id: "windows-console-process".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: Some("session-windows-close".to_owned()),
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();

    let error = close_session_state_with_error(
        &Store::open_unchecked(directory.path()),
        None,
        |_| {
            Err(anyhow::anyhow!(
                "failed to attach to the managed console process: Access is denied"
            ))
        },
        budget::close_deadline(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("Access is denied"));
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state
            .as_str(),
        "ready"
    );
}

#[test]
fn concurrent_close_requests_share_one_terminal_handle_claim() {
    use std::sync::{Arc, Barrier, atomic::AtomicUsize};

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "concurrent-close".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let first_directory = directory.path().to_owned();
    let first_barrier = Arc::clone(&barrier);
    let first_calls = Arc::clone(&calls);
    let first = std::thread::spawn(move || {
        close_session_state_with_error(
            &Store::open_unchecked(&first_directory),
            None,
            |_| {
                first_calls.fetch_add(1, Ordering::SeqCst);
                first_barrier.wait();
                std::thread::sleep(Duration::from_millis(50));
                Ok(terminal::CloseOutcome::Closed.into())
            },
            budget::close_deadline(),
        )
    });

    while !directory.path().join(TERMINAL_CLOSING_FILE).exists() {
        std::thread::yield_now();
    }
    barrier.wait();
    close_session_state_with_error(
        &Store::open_unchecked(directory.path()),
        None,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(terminal::CloseOutcome::Closed.into())
        },
        budget::close_deadline(),
    )
    .unwrap();
    first.join().unwrap().unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state
            .as_str(),
        "closed"
    );
}

#[test]
fn tell_cannot_claim_while_explicit_close_owns_the_terminal_lifecycle() {
    use std::sync::{Arc, Barrier};

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "closing-before-tell".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let close_directory = directory.path().to_owned();
    let close_barrier = Arc::clone(&barrier);
    let close = thread::spawn(move || {
        close_session_state_with_error(
            &Store::open_unchecked(&close_directory),
            None,
            |_| {
                close_barrier.wait();
                Ok(terminal::CloseOutcome::Closed.into())
            },
            budget::close_deadline(),
        )
    });

    barrier.wait();
    let tell_directory = directory.path().to_owned();
    let tell = thread::spawn(move || {
        turn::claim_ready(
            &Store::open_unchecked(&tell_directory),
            "session-close123",
            &[],
        )
    });
    close.join().unwrap().unwrap();
    let tell_error = match tell.join().unwrap() {
        Ok(_) => panic!("tell claimed a session while close owned its lifecycle"),
        Err(error) => error,
    };

    assert!(tell_error.to_string().contains("closed"));
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn claimed_and_awaiting_initial_input_dead_owners_respect_surface_cleanup() {
    for state in ["claimed", "awaiting-initial-input"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), state, reaped_child_pid());

        #[cfg(windows)]
        assert!(
            repair_dead_owner_with(
                &Store::open_unchecked(directory.path()),
                |owner| repair_owner_is_live(&attest_owner(owner)),
                terminal::surface_outlives_owner,
                |surface| terminal::ownership::close_dead_owner_surface_with(
                    surface,
                    &mut (|session: &terminal::TerminalSession| {
                        assert_eq!(session.kind, terminal::TerminalKind::WindowsConsole);
                        Ok(terminal::CloseOutcome::Missing)
                    }),
                    budget::close_deadline()
                ),
                budget::close_deadline()
            )
            .unwrap()
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(
            session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap()
        );
        #[cfg(target_os = "macos")]
        {
            assert!(
                !session_close::repair_dead_owner(&Store::open_unchecked(directory.path()))
                    .unwrap()
            );
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_eq!(status.state.as_str(), state);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_eq!(status.state.as_str(), "closed");
        }
    }
}

#[test]
fn explicit_close_converges_after_a_fault_before_every_close_mutation() {
    for journaled_event in [
        JournaledEventState::Absent,
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        explicit_close_converges_after_every_fault_with(journaled_event);
    }
}

#[test]
fn sessions_repairs_a_dead_owner_whose_journal_cannot_be_recovered() {
    for state in ["working", "exited"] {
        let fixture = seed_dead_owner_with_unrecoverable_journal(state);
        let directory = fixture.directory.clone();
        let label = format!("state {state}");
        // The missing directory is damage that completion recovery refuses.
        let error = recover_pending_completion(&directory).unwrap_err();
        assert!(
            format!("{error:#}").contains("events directory is missing"),
            "{label}: {error:#}"
        );

        // Repair does not stop at that damage: the owner is dead, so the session is
        // closed, which settles the journal the way every close settles a journal with
        // no event to verify, and the damage is reported in the close error.
        if state == "working" && cfg!(windows) {
            assert!(
                repair_dead_owner_with(
                    &Store::open_unchecked(&directory),
                    |owner| repair_owner_is_live(&attest_owner(owner)),
                    terminal::surface_outlives_owner,
                    |surface| terminal::ownership::close_dead_owner_surface_with(
                        surface,
                        &mut (|session: &terminal::TerminalSession| {
                            assert_eq!(session.kind, terminal::TerminalKind::WindowsConsole);
                            Ok(terminal::CloseOutcome::Missing)
                        }),
                        budget::close_deadline()
                    ),
                    budget::close_deadline()
                )
                .unwrap(),
                "{label}"
            );
        }
        let listing = cli_sessions(fixture.root.path());
        assert_eq!(listing.len(), 1, "{label}: {listing:?}");
        if cfg!(target_os = "macos") {
            // A dead owner and unreadable journal prove nothing about its window.
            assert_eq!(listing[0]["state"], state, "{label}: {listing:?}");
            assert!(directory.join(TERMINAL_HANDLE_FILE).exists());
            assert!(!directory.join(TERMINAL_TOMBSTONE_FILE).exists());
            assert!(!directory.join(CLOSED_STATUS_FILE).exists());
            assert!(directory.join(TURN_COMPLETION_FILE).exists());
            continue;
        }
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "closed", "{label}");
        let close_error = status.error.clone().unwrap_or_default();
        assert!(
            close_error.contains("is no longer running")
                && close_error.contains("events directory is missing"),
            "{label}: {close_error}"
        );
        assert!(directory.join(CLOSED_STATUS_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert!(!directory.join(TERMINAL_HANDLE_FILE).exists(), "{label}");
        assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists(), "{label}");
        assert!(
            !directory.join("events").exists(),
            "{label}: the repair created an events directory"
        );
        assert_eq!(
            request_state(&directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned()),
            "{label}"
        );

        // Explicit close of the repaired session is idempotent: no adapter call, no
        // replacement claim, no event recreated, the tombstone unchanged.
        let tombstone = fs::read(directory.join(CLOSED_STATUS_FILE)).unwrap();
        let mut adapter_calls = 0;
        for _ in 0..2 {
            {
                let closer =
                    &mut |_: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                        adapter_calls += 1;
                        Ok(terminal::CloseOutcome::Closed)
                    };
                session_close::close(
                    &Store::open_unchecked(&directory),
                    None,
                    session_close::budget::close_deadline(),
                    |surface| closer(surface).map(Into::into),
                )
            }
            .map(|_| ())
            .unwrap();
        }
        assert_eq!(adapter_calls, 0, "{label}");
        assert_eq!(
            fs::read(directory.join(CLOSED_STATUS_FILE)).unwrap(),
            tombstone,
            "{label}: the tombstone changed"
        );
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join("events").exists(), "{label}");
        assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
        assert!(
            !session_close::repair_dead_owner(&Store::open_unchecked(&directory)).unwrap(),
            "{label}"
        );
        assert_eq!(
            request_state(&directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned()),
            "{label}"
        );
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
#[test]
fn unsupported_platform_refuses_bound_close_and_restores_claimed_handle() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-abc");
    fs::create_dir(&directory).unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    let mut surface = close_test_terminal();
    surface.managed_session_id = Some("session-abc".into());
    write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &surface).unwrap();
    let original = fs::read(directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    assert!(!directory.join(SESSION_OWNER_FILE).exists());
    let authority = |handle: &terminal::TerminalSession| {
        let deadline = budget::close_deadline();
        terminal::close_authority::test_support::TestObservation::on_records(
            &directory,
            "session-abc",
            handle,
            deadline,
        )
        .decide("session-abc", handle, deadline)
    };
    assert!(
        authority(&surface)
            .unwrap_err()
            .to_string()
            .contains("no visible terminal transport")
    );
    let result = close_session_state_with_error(
        &Store::open_unchecked(&directory),
        None,
        |handle| {
            assert!(!directory.join(TERMINAL_HANDLE_FILE).exists());
            assert!(directory.join(TERMINAL_CLOSING_FILE).exists());
            authority(handle)?;
            panic!("unsupported platform must never reach close")
        },
        budget::close_deadline(),
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("no visible terminal transport"));
    assert_eq!(
        fs::read(directory.join(TERMINAL_HANDLE_FILE)).unwrap(),
        original
    );
    assert!(!directory.join(TERMINAL_CLOSING_FILE).exists());
    assert!(!directory.join(TERMINAL_TOMBSTONE_FILE).exists());
    assert!(!directory.join("closed.json").exists());
}

fn explicit_close_converges_after_every_fault_with(journaled_event: JournaledEventState) {
    let mut faulted = 0;
    let mut tombstoned = 0;
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_close_fixture(journaled_event);
        let CloseFixture {
            directory,
            request_id,
            pending,
            event_path,
            ..
        } = &fixture;
        let directory = directory.as_path();
        let mut adapter_calls = 0;

        let outcome = with_fault_budget(budget, || {
            close_session_state_with_error(
                &Store::open_unchecked(directory),
                Some("closed by the maintainer".to_owned()),
                |_| {
                    adapter_calls += 1;
                    Ok(terminal::CloseOutcome::Closed.into())
                },
                crate::native::session::close::budget::close_deadline(),
            )
        });
        match outcome {
            Ok(()) => break,
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
            }
        }
        faulted += 1;

        // Recovery under the lifecycle lock must finish an interrupted close whose
        // tombstone exists and must never leave a journal behind. Without a tombstone the
        // close never committed, so recovery treats the journal as an ordinary interrupted
        // completion: it publishes a matching or missing event and refuses a different one.
        let tombstoned_before_recovery = directory.join(CLOSED_STATUS_FILE).exists();
        let recovered = recover_pending_completion(directory);
        if tombstoned_before_recovery {
            tombstoned += 1;
            recovered.unwrap();
            assert_journal_settled(directory);
            let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
            assert_eq!(status.state.as_str(), "closed");
            assert_eq!(status.error.as_deref(), Some("closed by the maintainer"));
            assert!(!directory.join(TURN_CLAIM_FILE).exists());
            assert!(!directory.join(LEGACY_RESUME_PENDING_FILE).exists());
        } else if journaled_event == JournaledEventState::Mismatched {
            assert!(format!("{:#}", recovered.unwrap_err()).contains("contains different data"));
        } else {
            recovered.unwrap();
            assert_journal_settled(directory);
        }

        close_session_state_with_error(
            &Store::open_unchecked(directory),
            None,
            |_| {
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed.into())
            },
            budget::close_deadline(),
        )
        .unwrap();
        assert!(
            adapter_calls <= 2,
            "the terminal adapter ran {adapter_calls} times"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "closed");
        let tombstone: SessionStatus = read_json(&directory.join(CLOSED_STATUS_FILE)).unwrap();
        assert_eq!(tombstone.generation, status.generation);
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        assert!(!directory.join(TURN_COMPLETION_FILE).exists());
        assert!(!directory.join(LEGACY_RESUME_PENDING_FILE).exists());
        assert!(!directory.join(TERMINAL_HANDLE_FILE).exists());
        assert!(!directory.join(TERMINAL_CLOSING_FILE).exists());
        assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
        assert!(!recover_pending_completion(directory).unwrap());
        // Once the close committed, its commit point wins: a journal whose event was never
        // written is discarded. An event the completion already wrote is the provider's
        // result and stays published exactly when it matches its journal. A close that
        // never committed lets recovery publish the completion first.
        let (request, session) = request_state(directory, request_id);
        assert_eq!(session, "closed");
        let published = match journaled_event {
            JournaledEventState::Committed => true,
            JournaledEventState::Absent => !tombstoned_before_recovery,
            JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => false,
        };
        assert_eq!(
            request,
            if published { "completed" } else { "unresolved" },
            "{journaled_event:?} budget {budget}"
        );
        assert_eq!(
            event_paths(directory).unwrap(),
            if published {
                vec![event_path.clone()]
            } else {
                Vec::new()
            }
        );
        let quarantined = directory
            .join("events")
            .join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
        assert_eq!(
            quarantined.exists(),
            journaled_event == JournaledEventState::Mismatched
        );
    }
    assert!(
        faulted >= 20,
        "only {faulted} close mutation boundaries were exercised for {journaled_event:?}"
    );
    assert!(
        tombstoned >= 8,
        "interrupted post-tombstone cleanup was not exercised for {journaled_event:?}"
    );
    // The terminal handle is claimed by a direct rename with its own sync; both of that
    // pair's boundaries are fault points like every other rename's.
    for label in [
        "claiming the terminal handle for close",
        "syncing the claimed terminal handle's directory",
    ] {
        assert!(
            labels.iter().any(|error| error.contains(label)),
            "no fault before {label} for {journaled_event:?}: {labels:?}"
        );
    }
}

/// [`seed_close_fixture`] with a committed event, in `state`, owned by a process that has
/// already exited, and then stripped of its `events/` directory: a journal that completion
/// recovery can no longer settle on a session only repair can close.
fn seed_dead_owner_with_unrecoverable_journal(state: &str) -> CloseFixture {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    update_status(&fixture.directory, state.parse().unwrap(), None, None).unwrap();
    write_dead_owner(&fixture.directory);
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();
    fixture
}

fn write_dead_owner(directory: &Path) {
    let pid = reaped_child_pid();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            managed_session_id: Some("session-close123".to_owned()),
            windows_process_identity: test_windows_process_identity(pid).or_else(|| {
                cfg!(windows).then(|| terminal::WindowsProcessIdentity {
                    creation_time: 0,
                    executable_path: String::new(),
                })
            }),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

// The partial transition of an explicit Terminal.app close: the teardown ended the owner and
// the adapter close failed. The retry must reach the adapter instead of reporting the
// surface closed because the owner is gone.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_after_teardown_reaches_the_adapter() {
    for outcome in [
        terminal::CloseOutcome::Closed,
        terminal::CloseOutcome::Missing,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

        // A later command's repair does not report the surface closed.
        assert!(
            !session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap()
        );
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        // The close that recorded the intent may finish without signalling anything. The
        // first assertion keeps a regression from reaching the Terminal.app probe.
        assert!(
            session_close::terminal_close_resumable(
                &Reader::open_unchecked(directory.path()),
                &handle,
                terminal::surface_outlives_owner(&handle)
            )
            .unwrap()
        );
        assert!(
            {
                let deadline = crate::native::session::close::budget::close_deadline();
                let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
                    |_| {
                        let app = owner.terminal_app.as_ref().unwrap();
                        Ok(Some((app.start_seconds, app.start_microseconds)))
                    },
                    || Ok(vec![owner.terminal_app.clone().unwrap()]),
                );
                TestObservation::on_records(directory.path(), "session-owner123", &handle, deadline)
                    .with_presence(|| panic!("a matching intent does not need a surface query"))
                    .with_evidence(if handle.kind == terminal::TerminalKind::AppleTerminal {
                        &apple
                    } else {
                        terminal::close_evidence(handle.kind)
                    })
                    .decide("session-owner123", &handle, deadline)
            }
            .unwrap()
                == TerminalCloseAuthority::SurfaceOnly
        );

        let mut adapter_calls = 0;
        {
            let closer =
                &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                    adapter_calls += 1;
                    assert_eq!(session, &handle);
                    Ok(outcome)
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
        assert_eq!(adapter_calls, 1, "the retried close must reach the adapter");
        assert_closed_without_terminal_records(directory.path());
    }
}

// A legacy handle has only window/tty identity. Those names can belong to a new
// Terminal process after a restart; a prior close intent must not authorize it. The
// retry never reaches the adapter: only a window that is proven gone ends it.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_without_app_incarnation_never_reaches_the_adapter() {
    for present in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        owner.terminal_app = None;
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let path = directory.path().join(TERMINAL_HANDLE_FILE);
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession = read_json(&path).unwrap();
        assert!(
            session_close::terminal_close_resumable(
                &Reader::open_unchecked(directory.path()),
                &handle,
                terminal::surface_outlives_owner(&handle)
            )
            .unwrap()
        );
        let result = {
            let closer =
                &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                    let authority = {
                        let deadline = crate::native::session::close::budget::close_deadline();
                        let apple =
                            terminal::macos::apple_terminal::close_evidence::with_observations(
                                |_| panic!("no app incarnation is recorded"),
                                || Ok(Vec::new()),
                            );
                        TestObservation::on_records(
                            directory.path(),
                            "session-owner123",
                            session,
                            deadline,
                        )
                        .with_presence(|| Ok(present))
                        .with_evidence(if session.kind == terminal::TerminalKind::AppleTerminal {
                            &apple
                        } else {
                            terminal::close_evidence(session.kind)
                        })
                        .decide("session-owner123", session, deadline)
                    }?;
                    assert_eq!(
                        authority,
                        TerminalCloseAuthority::Absent,
                        "a legacy retry reached an adapter without app-incarnation proof"
                    );
                    Ok(terminal::CloseOutcome::Missing)
                };
            session_close::close(
                &Store::open_unchecked(directory.path()),
                None,
                session_close::budget::close_deadline(),
                |surface| closer(surface).map(Into::into),
            )
        }
        .map(|_| ());
        assert_eq!(result.is_ok(), !present, "{result:?}");
        if present {
            assert!(path.exists());
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        } else {
            assert_closed_without_terminal_records(directory.path());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_incarnation_gates_retry_and_read_only_absence() {
    for prior_intent in [false, true] {
        for mode in [
            "same-present",
            "same-absent",
            "dead",
            "reused",
            "unreadable",
            "missing",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
            let birth = (100, 42);
            owner.terminal_app = (mode != "missing").then_some(MacTerminalAppIdentity {
                pid: 1234,
                start_seconds: birth.0,
                start_microseconds: birth.1,
            });
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if prior_intent {
                fail_apple_terminal_close_after_teardown(directory.path(), &owner);
            }
            let observations = std::cell::Cell::new(0);
            let result = {
                let closer =
                    &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                        let authority = {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let apple =
                                terminal::macos::apple_terminal::close_evidence::with_observations(
                                    |pid| {
                                        assert_eq!(pid, 1234);
                                        match mode {
                                            "dead" => Ok(None),
                                            "reused" => Ok(Some((101, 42))),
                                            "unreadable" => bail!("injected OS observation error"),
                                            "missing" => {
                                                panic!("missing app identity must not query")
                                            }
                                            _ => Ok(Some(birth)),
                                        }
                                    }, // A record without an incarnation is not settled by a reply that a
                                    // second Terminal process could have given.
                                    || {
                                        let mut instances = vec![terminal_instance(1234, 100)];
                                        if mode == "missing" {
                                            instances.push(terminal_instance(5678, 200));
                                        }
                                        Ok(instances)
                                    },
                                );
                            TestObservation::on_records(
                                directory.path(),
                                "session-owner123",
                                session,
                                deadline,
                            )
                            .with_presence(|| {
                                observations.set(observations.get() + 1);
                                Ok(mode == "same-present")
                            })
                            .with_evidence(
                                if session.kind == terminal::TerminalKind::AppleTerminal {
                                    &apple
                                } else {
                                    terminal::close_evidence(session.kind)
                                },
                            )
                            .decide(
                                "session-owner123",
                                session,
                                deadline,
                            )
                        }?;
                        if authority == TerminalCloseAuthority::Absent {
                            Ok(terminal::CloseOutcome::Missing)
                        } else {
                            assert!(prior_intent);
                            assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
                            bail!("injected adapter failure retains retry")
                        }
                    };
                session_close::close(
                    &Store::open_unchecked(directory.path()),
                    None,
                    session_close::budget::close_deadline(),
                    |surface| closer(surface).map(Into::into),
                )
            }
            .map(|_| ());
            let absent = mode == "dead" || (!prior_intent && mode == "same-absent");
            assert_eq!(result.is_ok(), absent, "{prior_intent}/{mode}: {result:?}");
            if absent {
                assert_closed_without_terminal_records(directory.path());
            } else {
                assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
            }
            assert_eq!(
                observations.get(),
                usize::from(!prior_intent && mode.starts_with("same-")),
                "{prior_intent}/{mode}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_failure_or_interruption_keeps_the_intent_and_the_handle() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    fail_apple_terminal_close_after_teardown(directory.path(), &owner);

    {
        let closer = &mut |_: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
            Err(anyhow::anyhow!(
                "Agent Bridge Terminal.app window no longer holds its tab"
            ))
        };
        session_close::close(
            &Store::open_unchecked(directory.path()),
            None,
            session_close::budget::close_deadline(),
            |surface| closer(surface).map(Into::into),
        )
    }
    .map(|_| ())
    .unwrap_err();
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");

    // A closer that stopped after claiming the handle leaves it claimed; repair leaves it
    // and the next close resumes the claim.
    fs::rename(
        directory.path().join(TERMINAL_HANDLE_FILE),
        directory.path().join(TERMINAL_CLOSING_FILE),
    )
    .unwrap();
    assert!(!session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    assert!(directory.path().join(TERMINAL_CLOSING_FILE).exists());
    let mut adapter_calls = 0;
    {
        let closer = &mut |_: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
            adapter_calls += 1;
            Ok(terminal::CloseOutcome::Closed)
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
    assert_eq!(adapter_calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

// A Terminal.app record that names no app incarnation: a start that failed before the
// wrapper recorded itself, an owner record of 0.0.10 or earlier, and such a record whose
// close had begun. Nothing ties a reply to the app that created the window, so the window
// is gone only when one stable instance, or none, can have answered and its list lacks
// the window. Everything else keeps the handle, and nothing is ever closed.
#[cfg(target_os = "macos")]
#[test]
fn terminal_record_without_app_incarnation_is_consumed_only_by_proven_absence() {
    let mut failures = Vec::new();
    for shape in [
        "ownerless-failed-start",
        "legacy-dead-owner",
        "legacy-intent-retry",
    ] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "none-running-but-listed",
            "two-instances",
            "restarted",
            "started-meanwhile",
            "listing-error",
            "presence-error",
        ] {
            let case = format!("{shape}/{mode}");
            let directory = tempfile::tempdir().unwrap();
            if shape == "ownerless-failed-start" {
                write_failed_launch_with_owner(
                    directory.path(),
                    terminal::TerminalKind::AppleTerminal,
                    launch::Phase::Pending,
                    reaped_child_pid(),
                );
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            } else {
                let mut owner = write_attested_apple_terminal_state(
                    directory.path(),
                    "ready",
                    reaped_child_pid(),
                );
                owner.terminal_app = None;
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                if shape == "legacy-intent-retry" {
                    fail_apple_terminal_close_after_teardown(directory.path(), &owner);
                }
            }
            let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let listings = std::cell::Cell::new(0);
            let (mut observations, mut adapter_calls) = (0, 0);
            let result = {
                let closer =
                    &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                        let authority = {
                            let deadline = crate::native::session::close::budget::close_deadline();
                            let apple =
                                terminal::macos::apple_terminal::close_evidence::with_observations(
                                    |_| panic!("{case}: no app incarnation is recorded"),
                                    || {
                                        listings.set(listings.get() + 1);
                                        let first = listings.get() == 1;
                                        match mode {
                                            "listing-error" => {
                                                bail!("injected process table error")
                                            }
                                            "none-running" | "none-running-but-listed" => {
                                                Ok(Vec::new())
                                            }
                                            "two-instances" => Ok(vec![
                                                terminal_instance(1234, 100),
                                                terminal_instance(5678, 200),
                                            ]),
                                            "restarted" if !first => {
                                                Ok(vec![terminal_instance(1234, 101)])
                                            }
                                            "started-meanwhile" if first => Ok(Vec::new()),
                                            _ => Ok(vec![terminal_instance(1234, 100)]),
                                        }
                                    },
                                );
                            TestObservation::on_records(
                                directory.path(),
                                "session-owner123",
                                session,
                                deadline,
                            )
                            .with_presence(|| {
                                observations += 1;
                                match mode {
                                    "presence-error" => bail!("injected unreadable window list"),
                                    "one-stable-listed" | "none-running-but-listed" => Ok(true),
                                    _ => Ok(false),
                                }
                            })
                            .with_evidence(
                                if session.kind == terminal::TerminalKind::AppleTerminal {
                                    &apple
                                } else {
                                    terminal::close_evidence(session.kind)
                                },
                            )
                            .decide(
                                "session-owner123",
                                session,
                                deadline,
                            )
                        }?;
                        if authority == TerminalCloseAuthority::Absent {
                            return Ok(terminal::CloseOutcome::Missing);
                        }
                        adapter_calls += 1;
                        Ok(terminal::CloseOutcome::Closed)
                    };
                session_close::close(
                    &Store::open_unchecked(directory.path()),
                    None,
                    session_close::budget::close_deadline(),
                    |surface| closer(surface).map(Into::into),
                )
            }
            .map(|_| ());

            let absent = matches!(mode, "none-running" | "one-stable");
            let wanted_observations =
                usize::from(!matches!(mode, "two-instances" | "listing-error"));
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
            let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
            let intent_kept = directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists();
            let settled = if absent {
                result.is_ok() && !handle_kept && tombstone && status.state.as_str() == "closed"
            } else {
                result.is_err()
                    && handle_kept
                    && !tombstone
                    && status.state == before.state
                    && intent_kept == (shape == "legacy-intent-retry")
            };
            if !settled || adapter_calls != 0 || observations != wanted_observations {
                failures.push(format!(
                    "{case}: result {result:?}, state {}, handle kept {handle_kept}, {adapter_calls} close steps, {observations} observations",
                    status.state
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// Warp can close the owned tab (ending its owner) while its dedicated window remains.
// A second close must retain authority to verify the window, not consume the handle
// merely because the owner died during the first requested close.
#[cfg(target_os = "macos")]
#[test]
fn warp_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Warp;
    handle.id = "instance-test".into();
    handle.tab_id = Some("tab-test".into());
    handle.window_id = Some("window-test".into());
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(
        &Store::open_unchecked(directory.path()),
        None,
        |session| {
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory.path()),
                "session-owner123",
                session,
                &owner,
            )?;
            bail!("managed Warp tab is closed but its dedicated empty window remains")
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();

    assert!(!session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(
        session_close::terminal_close_resumable(
            &Reader::open_unchecked(directory.path()),
            &handle,
            terminal::surface_outlives_owner(&handle)
        )
        .unwrap()
    );
    assert!(
        {
            let deadline = crate::native::session::close::budget::close_deadline();
            TestObservation::on_records(directory.path(), "session-owner123", &handle, deadline)
                .decide("session-owner123", &handle, deadline)
        }
        .unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    {
        let closer = &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
            assert_eq!(session, &handle);
            calls += 1;
            Ok(terminal::CloseOutcome::Missing)
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
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

#[cfg(target_os = "macos")]
#[test]
fn wezterm_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::WezTerm;
    handle.id = "0".into();
    // As a handle is stored, so that the record keeps whatever scope it was created with.
    handle.wezterm_mux = Some(
        serde_json::from_value(serde_json::json!({
            "socket": "/tmp/owned-wezterm/gui-sock-123",
            "pid": 123,
            "start_seconds": 100,
            "start_microseconds": 1,
        }))
        .unwrap(),
    );
    handle.tab_id = None;
    handle.window_id = None;
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(
        &Store::open_unchecked(directory.path()),
        None,
        |session| {
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory.path()),
                "session-owner123",
                session,
                &owner,
            )?;
            bail!("managed WezTerm pane is closed but its process remains")
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();

    assert!(!session_close::repair_dead_owner(&Store::open_unchecked(directory.path())).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(
        session_close::terminal_close_resumable(
            &Reader::open_unchecked(directory.path()),
            &handle,
            terminal::surface_outlives_owner(&handle)
        )
        .unwrap()
    );
    assert!(
        {
            let deadline = crate::native::session::close::budget::close_deadline();
            TestObservation::on_records(directory.path(), "session-owner123", &handle, deadline)
                .decide("session-owner123", &handle, deadline)
        }
        .unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    {
        let closer = &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
            assert_eq!(session, &handle);
            calls += 1;
            Ok(terminal::CloseOutcome::Missing)
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
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

// An invalid intent grants no close authority, but is not evidence that the surface
// disappeared. An owner that died before any explicit close also leaves cleanup unverified.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_unless_it_names_this_handle_and_owner() {
    let record = |directory: &Path, id: &str, handle, owner| {
        session_close::record_terminal_close_intent(
            &Store::open_unchecked(directory),
            id,
            handle,
            owner,
        )
        .unwrap()
    };
    let cases: [(&str, CloseIntentMutation); 6] = [
        ("no intent", |_, _, _| {}),
        ("corrupt intent", |directory, _, _| {
            fs::write(directory.join(TERMINAL_CLOSE_INTENT_FILE), "{").unwrap()
        }),
        ("another session", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.managed_session_id = Some("session-other456".to_owned());
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory),
                "session-other456",
                &handle,
                owner,
            )
            .unwrap()
        }),
        ("another handle", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.window_id = Some("1002".to_owned());
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory),
                "session-owner123",
                &handle,
                owner,
            )
            .unwrap()
        }),
        ("another owner", |directory, handle, owner| {
            let mut owner = owner.clone();
            owner.process_start_microseconds = Some(43);
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory),
                "session-owner123",
                handle,
                &owner,
            )
            .unwrap()
        }),
        ("owner without its identity", |directory, handle, owner| {
            let owner = NativeSessionOwner {
                pid: owner.pid,
                managed_session_id: owner.managed_session_id.clone(),
                terminal_tty: owner.terminal_tty.clone(),
                ..NativeSessionOwner::default()
            };
            write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner).unwrap();
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory),
                "session-owner123",
                handle,
                &owner,
            )
            .unwrap()
        }),
    ];
    for (case, mutate) in cases {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        mutate(directory.path(), &handle, &owner);
        assert!(
            !session_close::terminal_close_resumable(
                &Reader::open_unchecked(directory.path()),
                &handle,
                terminal::surface_outlives_owner(&handle)
            )
            .unwrap(),
            "{case}"
        );
        assert!(
            !repair_dead_owner_with(
                &Store::open_unchecked(directory.path()),
                |owner| repair_owner_is_live(&attest_owner(owner)),
                terminal::surface_outlives_owner,
                |surface| terminal::ownership::close_dead_owner_surface_with(
                    surface,
                    &mut (|_: &terminal::TerminalSession| {
                        panic!("{case}: absent or invalid intent grants no adapter authority")
                    }),
                    budget::close_deadline()
                ),
                budget::close_deadline()
            )
            .unwrap(),
            "{case}: missing close evidence must not become cleanup success"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        if case != "no intent" {
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        }
        assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready", "{case}");
    }

    // Only a Terminal.app handle can carry the intent.
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Iterm2;
    record(directory.path(), "session-owner123", &handle, &owner);
    assert!(
        !session_close::terminal_close_resumable(
            &Reader::open_unchecked(directory.path()),
            &handle,
            terminal::surface_outlives_owner(&handle)
        )
        .unwrap()
    );
}

// What a close may touch is the scope its session was created with, as the handle stores
// it. The lifecycle reads no opening preference: the adapter receives the stored handle
// unchanged, and the retry of a close is authorized for that exact handle only, so the
// same target read with another scope is given nothing.
#[cfg(target_os = "macos")]
#[test]
fn close_uses_the_stored_creation_scope_of_a_wezterm_session() {
    for owns_gui in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = terminal::TerminalKind::WezTerm;
        handle.id = "7".into();
        handle.tab_id = Some("3".into());
        handle.window_id = Some("1".into());
        handle.wezterm_mux = Some(
            serde_json::from_value(serde_json::json!({
                "socket": "/tmp/owned-wezterm/gui-sock-123",
                "pid": 123,
                "start_seconds": 100,
                "start_microseconds": 1,
                "owns_gui": owns_gui,
            }))
            .unwrap(),
        );
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        close_session_state_with_error(
            &Store::open_unchecked(directory.path()),
            None,
            |session| {
                assert_eq!(session, &handle);
                session_close::record_terminal_close_intent(
                    &Store::open_unchecked(directory.path()),
                    "session-owner123",
                    session,
                    &owner,
                )?;
                bail!("managed WezTerm pane is closed but its tab remains")
            },
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap_err();

        let mut other_scope = handle.clone();
        other_scope.wezterm_mux.as_mut().unwrap().owns_gui = !owns_gui;
        assert!(
            session_close::terminal_close_intent_owner(
                &Reader::open_unchecked(directory.path()),
                &other_scope,
                terminal::surface_outlives_owner(&other_scope)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            session_close::terminal_close_intent_owner(
                &Reader::open_unchecked(directory.path()),
                &handle,
                terminal::surface_outlives_owner(&handle)
            )
            .unwrap()
            .is_some()
        );

        let mut closed = None;
        {
            let closer =
                &mut |session: &terminal::TerminalSession| -> Result<terminal::CloseOutcome> {
                    let authority = {
                        let deadline = crate::native::session::close::budget::close_deadline();
                        TestObservation::on_records(
                            directory.path(),
                            "session-owner123",
                            session,
                            deadline,
                        )
                        .with_presence(|| {
                            panic!("the recorded intent needs no surface observation")
                        })
                        .with_evidence(terminal::close_evidence(session.kind))
                        .decide("session-owner123", session, deadline)
                    }?;
                    assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
                    closed = Some(session.clone());
                    Ok(terminal::CloseOutcome::Closed)
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
        assert_eq!(closed, Some(handle));
        assert_closed_without_terminal_records(directory.path());
    }
}

// The first attempt of an explicit Terminal.app close, up to its failure: the owner and the
// surface were verified, the intent was recorded as the teardown records it, the teardown
// ended the owner (it is already gone here), and the adapter close failed.
#[cfg(target_os = "macos")]
fn fail_apple_terminal_close_after_teardown(directory: &Path, owner: &NativeSessionOwner) {
    let failure = close_session_state_with_error(
        &Store::open_unchecked(directory),
        None,
        |session| {
            session_close::record_terminal_close_intent(
                &Store::open_unchecked(directory),
                "session-owner123",
                session,
                owner,
            )?;
            Err(anyhow::anyhow!(
                "Terminal.app automation failed: AppleEvent timed out."
            ))
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert!(failure.to_string().contains("timed out"));
}

#[cfg(target_os = "macos")]
type CloseIntentMutation = fn(&Path, &terminal::TerminalSession, &NativeSessionOwner);

#[cfg(target_os = "macos")]
fn assert_closed_without_terminal_records(directory: &Path) {
    for record in [
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
        TERMINAL_CLOSE_INTENT_FILE,
    ] {
        assert!(!directory.join(record).exists(), "{record} was kept");
    }
    assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

#[cfg(target_os = "macos")]
fn terminal_instance(pid: u32, start_seconds: u64) -> MacTerminalAppIdentity {
    MacTerminalAppIdentity {
        pid,
        start_seconds,
        start_microseconds: 42,
    }
}

// A launch that failed after its wrapper recorded itself, as `ask`, the wrapper and its
// finalization leave the records: the claim, the launch receipt, the bound handle, the
// complete owner and the failed status. `Pending`: the wrapper failed before the spawn
// (the provider version check). `Spawning`: the spawn is uncertain and keeps the claim.
#[cfg(target_os = "macos")]
fn write_failed_launch_with_owner(
    directory: &Path,
    kind: terminal::TerminalKind,
    phase: launch::Phase,
    owner_pid: u32,
) -> NativeSessionOwner {
    let owner = write_attested_apple_terminal_state(directory, "launching", owner_pid);
    let token = turn::current_claim_token(&Reader::open_unchecked(directory))
        .unwrap()
        .unwrap();
    launch::begin(
        &Store::open_unchecked(directory),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    if phase == launch::Phase::Spawning {
        let mut record = launch::read(&Reader::open_unchecked(directory))
            .unwrap()
            .unwrap();
        record.phase = phase;
        write_json_atomic(&directory.join(launch::FILE), &record).unwrap();
    }
    launch::finalize(
        &Store::open_unchecked(directory),
        &Err(anyhow::anyhow!("provider version check failed")),
    )
    .unwrap();
    let mut handle: terminal::TerminalSession =
        read_json(&directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = kind;
    write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    owner
}

#[test]
fn close_cleanup_publishes_a_journaled_completion_whose_event_was_written() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-review2");
    let (request_id, event_path) = seed_event_written_completion(&directory, "late result");

    let before = request_state(&directory, &request_id);
    close_session_state_with_error(
        &Store::open_unchecked(&directory),
        None,
        |_| Ok(terminal::CloseOutcome::Closed.into()),
        budget::close_deadline(),
    )
    .unwrap();
    let after = request_state(&directory, &request_id);

    assert_eq!(before.0, "completed");
    assert_eq!(after, ("completed".to_owned(), "closed".to_owned()));
    assert!(event_path.exists());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
}

#[test]
fn close_keeps_but_never_publishes_an_event_that_disagrees_with_its_journal() {
    for interrupted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-review2");
        let (request_id, event_path) =
            seed_event_written_completion(&directory, "different result");
        if interrupted {
            update_status(&directory, SessionState::Closed, None, None).unwrap();
        }

        let before = request_state(&directory, &request_id);
        if interrupted {
            assert!(recover_pending_completion(&directory).unwrap());
        } else {
            close_session_state_with_error(
                &Store::open_unchecked(&directory),
                None,
                |_| Ok(terminal::CloseOutcome::Closed.into()),
                budget::close_deadline(),
            )
            .unwrap();
        }
        let after = request_state(&directory, &request_id);

        assert_eq!(before.0, "recovery_required", "interrupted={interrupted}");
        assert_eq!(
            after,
            ("unresolved".to_owned(), "closed".to_owned()),
            "interrupted={interrupted}"
        );
        assert!(!event_path.exists());
        let quarantined = directory.join("events").join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            event_path.file_name().unwrap().to_str().unwrap()
        ));
        let kept: SessionEvent = read_json(&quarantined).unwrap();
        assert_eq!(kept.message, "different result");
        assert!(event_paths(&directory).unwrap().is_empty());
        assert!(!directory.join(TURN_COMPLETION_FILE).exists());
        assert!(!recover_pending_completion(&directory).unwrap());
    }
}

#[test]
fn every_close_boundary_reports_a_committed_result_before_and_after_sessions() {
    for journaled_event in [
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        close_boundary_queries_agree_with(journaled_event);
    }
}

#[test]
fn a_journal_over_the_read_limit_is_never_published_whichever_write_it_stopped_after() {
    // A journal written before the size policy existed carries an event over the limit.
    // Recovery refuses to publish it whether the event file is absent or present, so the
    // interruption order cannot decide the outcome; a close then settles both the same way.
    let event = oversized_sample_event(&"x".repeat(EVENT_READ_LIMIT as usize));
    let event_bytes = serde_json::to_vec_pretty(&event).unwrap();
    let size = event_bytes.len() as u64;
    assert!(size > EVENT_READ_LIMIT);
    let mut outcomes = Vec::new();
    for event_written in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-legacy");
        let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
        let mut pending = PendingTurnCompletion::new(&claim_token, event.clone(), None).unwrap();
        pending.event_file = event_path.file_name().unwrap().to_str().unwrap().to_owned();
        write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
        if event_written {
            fs::write(&event_path, &event_bytes).unwrap();
        }
        let label = format!("event_written={event_written}");

        let error = recover_pending_completion(&directory).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "{size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
            )),
            "{label}: {text}"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "working", "{label}");
        assert!(directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert_eq!(event_path.exists(), event_written, "{label}");
        let before = request_state(&directory, &request_id);
        assert_eq!(before.0, "recovery_required", "{label}");

        close_session_state_with_error(
            &Store::open_unchecked(&directory),
            None,
            |_| Ok(terminal::CloseOutcome::Closed.into()),
            budget::close_deadline(),
        )
        .unwrap();
        assert!(!event_path.exists(), "{label}");
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert!(event_paths(&directory).unwrap().is_empty(), "{label}");
        outcomes.push((before, request_state(&directory, &request_id)));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(
        outcomes[0].1,
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn close_settles_a_journaled_completion_when_the_events_directory_is_missing() {
    // A journal with no `events` directory has no event to verify: the close keeps its
    // tombstone, releases the claim, and discards the journal, and the session can then
    // be listed, closed again, and pruned. Before this fix the directory check failed the
    // close after the tombstone, leaving claim and journal installed forever.
    for journaled_event in [JournaledEventState::Committed, JournaledEventState::Absent] {
        let fixture = seed_close_fixture(journaled_event);
        let label = format!("{journaled_event:?}");
        fs::remove_dir_all(fixture.directory.join("events")).unwrap();
        let close = || {
            close_session_state_with_error(
                &Store::open_unchecked(&fixture.directory),
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed.into()),
                crate::native::session::close::budget::close_deadline(),
            )
        };
        let assert_settled = |phase: &str| {
            assert!(
                fixture.directory.join(CLOSED_STATUS_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_CLAIM_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join("events").exists(),
                "{label} {phase}: the close created an events directory"
            );
            assert_eq!(
                request_state(&fixture.directory, &fixture.request_id),
                ("unresolved".to_owned(), "closed".to_owned()),
                "{label} {phase}"
            );
        };

        close().unwrap();
        assert_settled("after the close");
        let tombstone_before = fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap();
        let listing = cli_sessions(fixture.root.path());
        assert_eq!(listing.len(), 1, "{label}: {listing:?}");
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        assert_settled("after sessions");
        close().unwrap();
        assert_settled("after the second close");
        assert_eq!(
            fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap(),
            tombstone_before,
            "{label}: the tombstone changed"
        );
        assert_eq!(
            prune_closed_sessions(fixture.root.path(), u128::MAX).unwrap(),
            ["session-fault"],
            "{label}"
        );
        assert!(!fixture.directory.exists(), "{label}");
    }

    // An interrupted close (tombstone written, nothing else) converges the same way.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    update_status(
        &fixture.directory,
        SessionState::Closed,
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();
    assert!(recover_pending_completion(&fixture.directory).unwrap());
    assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&fixture.directory).unwrap());
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn close_still_rejects_a_linked_events_directory() {
    // Settling a missing directory must not weaken link rejection: a link at `events`
    // fails the close, nothing behind the link is opened or moved, and the claim and
    // journal stay installed as the evidence of the unsettled completion.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("events");
    fs::rename(fixture.directory.join("events"), &target).unwrap();
    if !link_directory(&target, &fixture.directory.join("events")) {
        eprintln!("skipped: this environment cannot create a directory link");
        return;
    }
    let error = close_session_state_with_error(
        &Store::open_unchecked(&fixture.directory),
        Some("closed by the maintainer".to_owned()),
        |_| Ok(terminal::CloseOutcome::Closed.into()),
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(target.join(&fixture.pending.event_file).exists());
    assert!(
        !target
            .join(format!(
                "{UNPUBLISHED_EVENT_PREFIX}{}",
                fixture.pending.event_file
            ))
            .exists()
    );
    // The interrupted-close convergence refuses the link the same way.
    let error = recover_pending_completion(&fixture.directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
}

#[test]
fn claim_free_recovery_refuses_an_equivalent_event_in_another_encoding() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-compact");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    // Journal, event, terminal status, and claim release all exist, but the event bytes
    // are compact JSON: the same value, not the journal's canonical write.
    let event_path = directory.join("events").join(&pending.event_file);
    fs::write(&event_path, serde_json::to_vec(&pending.event).unwrap()).unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    let stored: SessionEvent = read_json(&event_path).unwrap();
    assert_eq!(stored, pending.event);

    let before = request_state(&directory, &request_id);
    assert_eq!(before, ("recovery_required".to_owned(), "ready".to_owned()));
    let error = recover_pending_completion(&directory).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("claim-free pending completion event does not match its journal"),
        "{error:#}"
    );
    let listing = cli_sessions(root.path());
    assert_eq!(listing[0]["state"], "ready");
    assert_eq!(listing[0]["results"], 0);
    let reader = Reader::open_unchecked(&directory);
    assert_eq!(reader.published_result_count().unwrap(), 0);
    let status = query::status::test_value_in(root.path());
    assert!(status["sessions"][0]["latest_result"].is_null());
    assert_eq!(request_state(&directory, &request_id), before);
    assert!(directory.join(TURN_COMPLETION_FILE).exists());
    assert!(event_path.exists());

    // Close settles the record the way it settles every event the journal disagrees
    // with: set aside under a name no query reads, never published.
    close_session_state_with_error(
        &Store::open_unchecked(&directory),
        None,
        |_| Ok(terminal::CloseOutcome::Closed.into()),
        budget::close_deadline(),
    )
    .unwrap();
    assert_eq!(
        request_state(&directory, &request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
    assert!(!event_path.exists());
    let quarantined = directory
        .join("events")
        .join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
    let kept: SessionEvent = read_json(&quarantined).unwrap();
    assert_eq!(kept, pending.event);
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
    assert_eq!(cli_sessions(root.path())[0]["results"], 0);
}

#[test]
fn recovery_syncs_an_interrupted_quarantine_before_discarding_its_journal() {
    // A close that stops right after moving a mismatched event aside, before `events/` is
    // synced, leaves the rename unsynced while the journal still proves the event was
    // unverified. The recovery that finishes the close must sync `events/` before it
    // removes the claim and the journal, or a later crash could bring the event back
    // without the journal that keeps it unpublished.
    let mut reproduced = false;
    for budget in 0.. {
        let fixture = seed_close_fixture(JournaledEventState::Mismatched);
        let events = fixture.directory.join("events");
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &Store::open_unchecked(&fixture.directory),
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed.into()),
                    crate::native::session::close::budget::close_deadline(),
                )
            })
        });
        let error = match outcome {
            Ok(()) => break,
            Err(error) => error,
        };
        assert!(injected_fault(&error), "{error:#}");
        let quarantined = events.join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            fixture.pending.event_file
        ));
        // The boundary under test: the event was moved aside, and `events/` was not
        // synced afterwards. (The close reports the later cleanup steps' faults, so the
        // boundary is recognised by its state, not by the error text.)
        if !quarantined.exists() || close_log.contains(&SyncRecord::Directory(events.clone())) {
            continue;
        }
        reproduced = true;
        assert!(!fixture.event_path.exists());
        assert!(fixture.directory.join(CLOSED_STATUS_FILE).exists());
        assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());

        let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(changed.unwrap());
        let events_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(events.clone()));
        let session_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
        assert!(
            matches!(
                (events_synced, session_synced),
                (Some(events), Some(session)) if events < session
            ),
            "the quarantine rename was not made durable before the session records changed: {log:?}"
        );
        assert!(quarantined.exists());
        assert!(!fixture.event_path.exists());
        assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned())
        );
        // Converged: a second recovery changes nothing and syncs nothing.
        let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(!again.unwrap());
        assert!(log.is_empty(), "{log:?}");
        break;
    }
    assert!(
        reproduced,
        "no close boundary stopped between the quarantine rename and its sync"
    );
}

#[test]
fn close_syncs_a_committed_event_before_discarding_its_journal() {
    // A close of the same interrupted session writes its tombstone first, so the events
    // sync cannot precede every session-directory sync; the property is that no
    // interruption of the close, nor of the recovery that finishes an interrupted close,
    // removes the claim or the journal before `events/` was synced. The finished close
    // leaves the event published and nothing for a later recovery to sync. The close
    // reports the later cleanup steps' faults rather than the barrier's own, so the
    // boundary is recognised by its state: tombstone written, claim and journal kept,
    // `events/` not yet synced.
    let mut stopped_before_barrier = false;
    for budget in 0.. {
        let fixture = seed_completion_stopped_before_its_events_sync();
        let events = fixture.directory.join("events");
        let claim_path = fixture.directory.join(TURN_CLAIM_FILE);
        let completion_path = fixture.directory.join(TURN_COMPLETION_FILE);
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &Store::open_unchecked(&fixture.directory),
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed.into()),
                    crate::native::session::close::budget::close_deadline(),
                )
            })
        });
        let events_synced = close_log.contains(&SyncRecord::Directory(events.clone()));
        if !(claim_path.exists() && completion_path.exists()) {
            assert!(
                events_synced,
                "budget {budget}: close discarded the journal's evidence before syncing events/: {close_log:?}"
            );
        }
        match outcome {
            Ok(()) => {
                assert!(events_synced);
                assert!(!claim_path.exists());
                assert!(!completion_path.exists());
                assert!(fixture.event_path.exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "closed".to_owned())
                );
                let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
                assert!(!again.unwrap());
                assert!(log.is_empty(), "{log:?}");
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
            }
        }
        // Between the status rewrite and the barrier: the recovery that finishes this
        // close has no session record to rewrite before it reaches the barrier.
        let at_barrier = fixture.directory.join(CLOSED_STATUS_FILE).exists()
            && read_json::<SessionStatus>(&fixture.directory.join("status.json"))
                .is_ok_and(|status| status.state.as_str() == "closed")
            && claim_path.exists()
            && completion_path.exists()
            && !events_synced;
        stopped_before_barrier |= at_barrier;
        // The next lifecycle-lock holder finishes the interrupted close (or, before the
        // tombstone, publishes the completion); either way it must not discard the
        // journal without the barrier having run in one of the two passes.
        let (recovered, recovery_log) =
            with_sync_log(|| recover_pending_completion(&fixture.directory));
        recovered.unwrap();
        assert!(!completion_path.exists());
        assert!(!claim_path.exists());
        assert!(
            events_synced || recovery_log.contains(&SyncRecord::Directory(events.clone())),
            "budget {budget}: neither the close nor its recovery synced events/: {close_log:?} {recovery_log:?}"
        );
        if at_barrier {
            let events_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(events.clone()));
            let session_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
            assert!(
                matches!(
                    (events_synced, session_synced),
                    (Some(events), Some(session)) if events < session
                ),
                "budget {budget}: the interrupted close's recovery changed session records before syncing events/: {recovery_log:?}"
            );
        }
        assert!(fixture.event_path.exists());
        // Before the tombstone the close never committed, so recovery published the
        // completion on a session that stays ready; after it, the close is finished.
        let session_state = if fixture.directory.join(CLOSED_STATUS_FILE).exists() {
            "closed"
        } else {
            "ready"
        };
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("completed".to_owned(), session_state.to_owned()),
            "budget {budget}"
        );
    }
    assert!(
        stopped_before_barrier,
        "no close boundary stopped after the tombstone and before the committed event's directory sync"
    );
}

/// `--context-result <address>` resolution as `ask` and `tell` perform it: the prompt the
/// target would receive, or the resolution error.
fn cli_context_result(root: &Path, address: &str) -> Result<String> {
    let mut references = Vec::new();
    context::push_option(&mut references, address)?;
    Ok(context::resolve_in(root, &references)?.prompt_with_attachments("continue"))
}

/// Stops the close before every filesystem mutation and, without recovering first, runs
/// `result`, `result --wait`, `search`, and a `--context-result` resolution over the
/// interrupted records. A committed event is reported as `completed` at every boundary,
/// before and after `sessions` converges the close; a mismatched event never is.
fn close_boundary_queries_agree_with(journaled_event: JournaledEventState) {
    let committed = journaled_event == JournaledEventState::Committed;
    let mut faulted = 0;
    let mut claim_released_before_journal = 0;
    for budget in 0.. {
        let fixture = seed_close_fixture(journaled_event);
        let root = fixture.root.path();
        let outcome = with_fault_budget(budget, || {
            close_session_state_with_error(
                &Store::open_unchecked(&fixture.directory),
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed.into()),
                crate::native::session::close::budget::close_deadline(),
            )
        });
        match outcome {
            Ok(()) => break,
            Err(error) => assert!(injected_fault(&error), "{error:#}"),
        }
        faulted += 1;
        let label = format!("{journaled_event:?} budget {budget}");

        // The claim is released before the journal is removed, so no boundary leaves a
        // claim that hides a committed event without the journal that proves it.
        let claim_present = fixture.directory.join(TURN_CLAIM_FILE).exists();
        let journal_present = fixture.directory.join(TURN_COMPLETION_FILE).exists();
        assert!(
            journal_present || !claim_present,
            "{label}: the claim outlived the completion journal"
        );
        if journal_present && !claim_present {
            claim_released_before_journal += 1;
        }
        let tombstoned = fixture.directory.join(CLOSED_STATUS_FILE).exists();

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} before"));

        let listing = cli_sessions(root);
        assert_eq!(listing.len(), 1, "{label}");
        assert_eq!(listing[0]["id"], "session-fault");
        // Without a tombstone the close never committed: `sessions` recovers the journal
        // as an ordinary completion instead, which publishes a committed event and
        // refuses a mismatched one.
        let expected_session_state = match (tombstoned, committed) {
            (true, _) => "closed",
            (false, true) => "ready",
            (false, false) => "working",
        };
        assert_eq!(listing[0]["state"], expected_session_state, "{label}");

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} after"));
        let after = cli_result(
            root,
            &[
                "result",
                "session-fault",
                "--request",
                &fixture.request_id,
                "--json",
            ],
        );
        assert_eq!(after["session_state"], expected_session_state, "{label}");
        assert_eq!(
            after["recovery_required"],
            !tombstoned && !committed,
            "{label}"
        );
        if tombstoned {
            assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists(), "{label}");
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label}"
            );
        }
    }
    assert!(
        faulted >= 20,
        "only {faulted} close boundaries were exercised for {journaled_event:?}"
    );
    assert!(
        claim_released_before_journal >= 2,
        "the boundaries between claim release and journal removal were not exercised for {journaled_event:?}"
    );
}

fn assert_close_boundary_queries(
    root: &Path,
    fixture: &CloseFixture,
    committed: bool,
    label: &str,
) {
    let id = "session-fault";
    let request_id = fixture.request_id.as_str();
    let plain = cli_result(root, &["result", id, "--request", request_id, "--json"]);
    let waited = cli_result(
        root,
        &[
            "result",
            id,
            "--request",
            request_id,
            "--wait",
            "--timeout-secs",
            "1",
            "--json",
        ],
    );
    let search = cli_search(
        root,
        &["search", "late result", "--all-workspaces", "--json"],
    );
    let context = cli_context_result(root, &format!("{id}/{request_id}"));
    if committed {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert_eq!(value["ok"], true, "{label}: {name}: {value}");
            assert_eq!(
                value["request_state"], "completed",
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], "late result", "{label}: {name}");
            assert_eq!(
                value["event_id"], fixture.pending.event_file,
                "{label}: {name}"
            );
            assert_eq!(
                value["timed_out"],
                serde_json::Value::Null,
                "{label}: {name}"
            );
        }
        let hits = search["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{label}: {search}");
        assert_eq!(hits[0]["session"], id, "{label}");
        assert_eq!(hits[0]["request_id"], request_id, "{label}");
        assert_eq!(hits[0]["event_id"], fixture.pending.event_file, "{label}");
        assert_eq!(search["incomplete"], false, "{label}: {search}");
        assert_eq!(search["scanned"]["events"], 1, "{label}: {search}");
        let prompt = context.unwrap_or_else(|error| panic!("{label}: {error:#}"));
        assert!(prompt.contains("late result"), "{label}: {prompt}");
    } else {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert!(
                matches!(
                    value["request_state"].as_str(),
                    Some("recovery_required" | "unresolved")
                ),
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], serde_json::Value::Null, "{label}: {name}");
        }
        assert!(
            search["hits"].as_array().unwrap().is_empty(),
            "{label}: {search}"
        );
        // While the journal and the differing event are both still in place, the scan
        // read the event to decide publication, skipped it, and says so.
        let unverified_in_place =
            fixture.directory.join(TURN_COMPLETION_FILE).exists() && fixture.event_path.exists();
        let reasons = search["incomplete_reasons"].to_string();
        assert_eq!(
            reasons.contains("differs from its pending completion journal"),
            unverified_in_place,
            "{label}: {reasons}"
        );
        assert_eq!(
            search["incomplete"], unverified_in_place,
            "{label}: {search}"
        );
        assert_eq!(
            search["scanned"]["events"],
            u64::from(unverified_in_place),
            "{label}: {search}"
        );
        let error = context.unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot be attached"),
            "{label}: {error:#}"
        );
    }
}

/// A completion stopped at the write boundary between the event's rename and the sync of
/// its directory entry: the event stands at its journal's path and matches it byte for
/// byte, the claim and the journal are still installed, and `events/` has not been synced
/// since the rename. Without a barrier, both recovery and close would accept the event as
/// committed and durably remove the journal, and a later crash could then lose the
/// event's directory entry together with the only evidence that it was the result.
struct UnsyncedCommittedEvent {
    _root: tempfile::TempDir,
    directory: PathBuf,
    request_id: String,
    event_path: PathBuf,
}

fn seed_completion_stopped_before_its_events_sync() -> UnsyncedCommittedEvent {
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fault");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
        let claim_token = claim.token().to_owned();
        let request_id = claim.receipt().request_id.clone();
        let event_path = directory.join("events").join(&claim.receipt().event_file);
        claim.retain();
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                turn::Report::for_claim(
                    &Store::open_unchecked(&directory),
                    FirstPartyCli::Codex,
                    Some(&claim_token),
                )
                .complete(
                    "committed result",
                    Some("provider-session".to_owned()),
                    Some("provider-turn".to_owned()),
                )
            })
        });
        let error =
            outcome.expect_err("the completion finished before it stopped between rename and sync");
        assert!(injected_fault(&error), "{error:#}");
        // The boundary is recognised by its state: the event exists at its final path and
        // no sync of `events/` has happened. Every earlier budget stops before the event's
        // rename; every later one has synced the directory.
        if !event_path.exists() || log.contains(&SyncRecord::Directory(directory.join("events"))) {
            continue;
        }
        assert!(directory.join(TURN_CLAIM_FILE).exists());
        assert!(directory.join(TURN_COMPLETION_FILE).exists());
        return UnsyncedCommittedEvent {
            _root: root,
            directory,
            request_id,
            event_path,
        };
    }
    unreachable!("the fault budget sweep never ends without returning or panicking")
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

    assert!(mark_session_closed(&Store::open_unchecked(directory.path()), None).is_err());

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

    assert!(mark_session_closed(&Store::open_unchecked(directory.path()), None).is_err());

    assert!(!directory.path().join(LEGACY_RESUME_RUNNING_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

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
fn close_fixtures_keep_independent_publication_expectations() {
    for state in [
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        let committed = state == JournaledEventState::Committed;
        for budget in 0.. {
            let fixture = seed_close_fixture(state);
            let outcome = crate::native::with_fault_budget(budget, || {
                close_session_state_with_error(
                    &Store::open_unchecked(&fixture.directory),
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed.into()),
                    crate::native::session::close::budget::close_deadline(),
                )
            });
            let finished = outcome.is_ok();
            if let Err(error) = outcome {
                assert!(crate::native::injected_fault(&error));
            }
            let reader = Reader::open_unchecked(&fixture.directory);
            let before = entries(&fixture.directory);
            let observation = Observation::read(&reader).unwrap();
            let inspect = query::inspect_value(&reader, "session-fault").unwrap();
            let (checks, doctor) = doctor_records(&reader);
            assert_eq!(reason(&checks, "session_records"), "records_read");
            assert_eq!(inspect["stored_state"], doctor["stored_state"]);
            assert_eq!(inspect["recovery_required"], doctor["recovery_required"]);
            assert_eq!(
                inspect["latest_result"],
                ResultReference::latest(&observation.records, &reader)
                    .unwrap()
                    .value()
            );
            assert_eq!(
                doctor["stored_state"],
                json!(observation.records.status.state)
            );
            assert_eq!(
                doctor["active_request_id"],
                json!(observation.active_request().map(|r| &r.request_id))
            );
            assert_eq!(
                doctor["active_request_state"],
                json!(
                    observation
                        .judgments
                        .active_state
                        .as_ref()
                        .map(|s| s.as_ref().unwrap().as_str())
                )
            );
            assert_eq!(
                inspect["latest_result"]["request_state"],
                if committed {
                    "completed"
                } else {
                    "unavailable"
                }
            );
            assert_eq!(entries(&fixture.directory), before);
            if finished {
                break;
            }
        }
    }
}

fn doctor_records(reader: &Reader) -> (Value, Value) {
    let mut checks = Vec::new();
    let mut observations = json!({});
    doctor::record_checks(
        reader,
        "session-fault",
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        &mut checks,
        &mut observations,
    );
    (serde_json::to_value(checks).unwrap(), observations)
}

fn reason<'a>(checks: &'a Value, id: &str) -> &'a str {
    checks
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["reason_code"]
        .as_str()
        .unwrap()
}

fn reaped_child_pid() -> u32 {
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn write_owned_terminal_state(directory: &Path, state: &str, owner_pid: u32) {
    fs::create_dir(directory.join("events")).unwrap();
    update_status(directory, state.parse().unwrap(), None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    claim.retain();
    let windows_process_identity = test_windows_process_identity(owner_pid).or_else(|| {
        cfg!(windows).then(|| terminal::WindowsProcessIdentity {
            creation_time: 0,
            executable_path: String::new(),
        })
    });
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: if cfg!(windows) {
                terminal::TerminalKind::WindowsConsole
            } else {
                terminal::TerminalKind::AppleTerminal
            },
            id: if cfg!(windows) {
                owner_pid.to_string()
            } else {
                "/dev/ttys999".to_owned()
            },
            tab_id: None,
            window_id: (!cfg!(windows)).then(|| "1001".to_owned()),
            managed_session_id: Some("session-owner123".to_owned()),
            wezterm_mux: None,
            windows_process_identity: windows_process_identity.clone(),
        },
    )
    .unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: owner_pid,
            managed_session_id: Some("session-owner123".to_owned()),
            terminal_tty: (!cfg!(windows)).then(|| "/dev/ttys999".to_owned()),
            windows_process_identity,
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

fn sample_completion(claim_token: &str, message: &str) -> PendingTurnCompletion {
    PendingTurnCompletion::new(
        claim_token,
        SessionEvent {
            cancelled: false,
            provider: FirstPartyCli::Codex.as_str().to_owned(),
            message: message.to_owned(),
            error: None,
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            created_unix_ms: Some(1),
        },
        None,
    )
    .unwrap()
}

/// After recovery no journal remains. Temporary files an injected fault left behind (as an
/// abrupt stop would) are best-effort records: observation must ignore them.
fn assert_journal_settled(directory: &Path) {
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    query::observe_snapshot(&Reader::open_unchecked(directory)).unwrap();
}

fn close_test_terminal() -> terminal::TerminalSession {
    terminal::TerminalSession {
        kind: if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::AppleTerminal
        },
        id: "4242".to_owned(),
        tab_id: None,
        window_id: None,
        managed_session_id: Some("session-close123".to_owned()),
        wezterm_mux: None,
        windows_process_identity: None,
    }
}

fn write_test_manifest(directory: &Path) {
    let id = directory.file_name().unwrap().to_str().unwrap().to_owned();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id,
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("/opt/codex"),
            provider_version: "codex-cli 0.147.0".to_owned(),
            workspace: directory.to_path_buf(),
            title: "review round 2".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
}

/// A completion that stopped right after writing its event: journal and event exist, the
/// claim is still held, and the status was never updated. Returns the request address and
/// the journal's event path. `event_message` lets the written event disagree with the journal.
fn seed_event_written_completion(directory: &Path, event_message: &str) -> (String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}

fn request_state(directory: &Path, request_id: &str) -> (String, String) {
    let value = query::request_result(&Reader::open_unchecked(directory), request_id).unwrap();
    (
        value["request_state"].as_str().unwrap().to_owned(),
        value["session_state"].as_str().unwrap().to_owned(),
    )
}

struct CloseFixture {
    root: tempfile::TempDir,
    directory: PathBuf,
    request_id: String,
    pending: PendingTurnCompletion,
    event_path: PathBuf,
}

/// A working session under `session-fault` with a held claim, a journaled completion
/// whose event is absent, committed, or mismatched, a terminal handle, and a legacy
/// resume marker: everything an explicit close has to settle.
fn seed_close_fixture(journaled_event: JournaledEventState) -> CloseFixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-fault");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    match journaled_event {
        JournaledEventState::Absent => {}
        JournaledEventState::Committed => {
            write_json_atomic(&event_path, &pending.event).unwrap();
        }
        JournaledEventState::Mismatched => {
            let mut event = pending.event.clone();
            event.message = "different result".to_owned();
            write_json_atomic(&event_path, &event).unwrap();
        }
        JournaledEventState::Oversized(_) => unreachable!("fixtures never exceed the read limit"),
    }
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &close_test_terminal(),
    )
    .unwrap();
    fs::write(directory.join(LEGACY_RESUME_PENDING_FILE), "{}").unwrap();
    CloseFixture {
        root,
        directory,
        request_id,
        pending,
        event_path,
    }
}

// The query commands exactly as the CLI runs them, parsed from argv and evaluated over
// an explicit state root instead of the process environment.
fn cli_result(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Result(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a result command")
    };
    query::result_value_in(root, &request).unwrap()
}

fn cli_search(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Search(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a search command")
    };
    query::search_value_in(root, &request).unwrap()
}

fn cli_sessions(root: &Path) -> Vec<serde_json::Value> {
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!("not a sessions command")
    };
    sessions_in(root, &request).unwrap()
}

fn oversized_sample_event(message: &str) -> SessionEvent {
    SessionEvent {
        cancelled: false,
        provider: FirstPartyCli::Codex.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: Some(1),
    }
}

/// A working session with a held claim and no completion yet: what a provider completion
/// commits into. Returns the request address, the claim token, and the receipt's event path.
fn seed_claimed_session(directory: &Path) -> (String, String, PathBuf) {
    let (request, claimed, path) = seed_claimed_session_with_claim(directory);
    let token = claimed.token().to_owned();
    claimed.retain();
    (request, token, path)
}

/// Points `link` at `target` as a directory symlink, or on Windows a junction when
/// symlinks need a privilege. Returns false where neither can be created.
fn link_directory(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link).is_ok()
        || Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|output| output.status.success());
    created && fs::symlink_metadata(link).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn seed_claimed_session_with_claim(directory: &Path) -> (String, turn::Claim, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let event_path = directory.join("events").join(&claim.receipt().event_file);
    (request_id, claim, event_path)
}

fn entries(directory: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            entries.push((entry.path(), None));
            entries.extend(self::entries(&entry.path()));
        } else {
            entries.push((entry.path(), Some(fs::read(entry.path()).unwrap())));
        }
    }
    entries.sort();
    entries
}

type Entry = (PathBuf, Option<Vec<u8>>);
