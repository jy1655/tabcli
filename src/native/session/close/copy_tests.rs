use super::*;
use crate::native::session::primitives::{with_fault_at, with_step_at};
use crate::native::session::test_support::lifecycle::seed_event_written_completion;
use crate::native::terminal::{TerminalKind, TerminalSession};
use serde_json::{Value, json};

const ID: &str = "session-copy";

fn foreign_kind() -> TerminalKind {
    if cfg!(windows) {
        TerminalKind::Tmux
    } else {
        TerminalKind::WindowsConsole
    }
}

fn foreign_owner(id: &str) -> NativeSessionOwner {
    let mut owner = NativeSessionOwner {
        pid: std::process::id(),
        managed_session_id: Some(id.into()),
        ..Default::default()
    };
    if cfg!(windows) {
        owner.process_start_seconds = Some(7);
    } else {
        owner.windows_process_identity = Some(terminal::WindowsProcessIdentity {
            creation_time: 1,
            executable_path: "foreign.exe".into(),
        });
    }
    owner
}

fn fixture(state: SessionState) -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(ID);
    fs::create_dir_all(dir.join("events")).unwrap();
    let store = Store::open_unchecked(&dir);
    update_status(&dir, state, None, Some("provider diagnosis".into())).unwrap();
    (root, store)
}

fn handle(kind: TerminalKind) -> TerminalSession {
    TerminalSession::new(kind, "surface".into()).with_binding(Some(ID.into()))
}

fn herdr_wire() -> Value {
    let mut core = json!({"socket":"unused","core_id":"core","created_ms":1,"host_pid":42});
    if cfg!(windows) {
        core["host_start_seconds"] = 1.into();
        core["host_start_microseconds"] = 2.into();
    } else {
        core["host_creation_time"] = 1.into();
    }
    json!([
        "herdr", "surface", null, null, ID, null, null, null, null, core
    ])
}

fn attempt(store: &Store) -> Result<terminal::CloseOutcome> {
    close_with_attestation(
        store,
        None,
        budget::close_deadline(),
        |_| panic!("copy owner observed"),
        |_| bail!("scripted adapter dispatch"),
    )
}

fn snapshot(store: &Store) -> Vec<Option<Vec<u8>>> {
    [
        CoreRecord::Terminal,
        CoreRecord::TerminalClosing,
        CoreRecord::TerminalClosed,
        CoreRecord::Closed,
        CoreRecord::Status,
        CoreRecord::TurnClaim,
        CoreRecord::Completion,
    ]
    .map(|name| store.record(name).bytes().unwrap())
    .to_vec()
}

fn invalid_claim(state: SessionState, marker: bool) {
    for bad in [
        b"{".to_vec(),
        serde_json::to_vec(&handle(foreign_kind()).with_binding(Some("session-other".into())))
            .unwrap(),
    ] {
        let (_root, store) = fixture(state.clone());
        store.write_terminal(&handle(foreign_kind())).unwrap();
        store
            .record(CoreRecord::TerminalClosing)
            .write_private(&bad)
            .unwrap();
        store
            .record(CoreRecord::Completion)
            .write_private(b"{")
            .unwrap();
        store
            .record(CoreRecord::TurnClaim)
            .write_private(b"123-2-1")
            .unwrap();
        if marker {
            store
                .write_terminal_closed(&json!({"consumed":true,"terminal":foreign_kind()}))
                .unwrap();
        }
        let before = snapshot(&store);
        for _ in 0..2 {
            let result = attempt(&store);
            assert_eq!(
                snapshot(&store),
                before,
                "records changed while rejecting a claimed handle"
            );
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains(
                    &store
                        .record(CoreRecord::TerminalClosing)
                        .path()
                        .display()
                        .to_string()
                ),
                "{error}"
            );
        }
    }
}

#[test]
fn failed_copy_validates_both_slots() {
    invalid_claim(SessionState::Failed, false);
}
#[test]
fn closed_copy_validates_both_slots() {
    invalid_claim(SessionState::Closed, false);
}
#[test]
fn committed_copy_validates_both_slots() {
    invalid_claim(SessionState::Failed, true);
}

#[test]
fn failed_marker_publication_keeps_both_copy_handles() {
    check_failed_marker(false);
}
#[test]
fn obstructed_marker_keeps_both_copy_handles() {
    check_failed_marker(true);
}
fn check_failed_marker(obstruction: bool) {
    let (_root, store) = fixture(SessionState::Failed);
    let active = handle(foreign_kind());
    store.write_terminal(&active).unwrap();
    store
        .record(CoreRecord::TerminalClosing)
        .write_json(&active)
        .unwrap();
    let marker = store.record(CoreRecord::TerminalClosed);
    let result = if obstruction {
        fs::create_dir(marker.path()).unwrap();
        attempt(&store)
    } else {
        with_fault_at("renaming a temporary record over its final path", || {
            attempt(&store)
        })
    };
    assert!(result.is_err());
    assert!(
        store.record(CoreRecord::Terminal).present().unwrap(),
        "lost active copy evidence"
    );
    assert!(
        store.record(CoreRecord::TerminalClosing).present().unwrap(),
        "lost validated claim"
    );
    assert!(store.closed_if_present().unwrap().is_none());
    if obstruction {
        fs::remove_dir(marker.path()).unwrap();
    }
    attempt(&store).unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
    assert!(
        store
            .status()
            .unwrap()
            .error
            .unwrap()
            .split("; ")
            .any(|r| r == "provider diagnosis")
    );
}

#[test]
fn replacement_copy_is_never_dispatched() {
    let (_root, store) = fixture(SessionState::Ready);
    store.write_terminal(&handle(TerminalKind::Herdr)).unwrap();
    let replacement = handle(foreign_kind());
    let path = store.record(CoreRecord::Terminal).path().to_owned();
    let mut calls = 0;
    let mut run = || {
        close(&store, None, budget::close_deadline(), |_| {
            calls += 1;
            Ok(terminal::CloseOutcome::Missing.into())
        })
    };
    let expected = replacement.clone();
    let first = with_step_at(
        "claiming the terminal handle for close",
        move || {
            crate::native::session::primitives::write_json_atomic(&path, &replacement).unwrap();
        },
        &mut run,
    );
    let first = first.expect_err("first attempt dispatched the replacement");
    assert_eq!(
        crate::native::failure_kind::classify(&first),
        Some(crate::native::failure_kind::FailureKind::Unsupported)
    );
    assert_eq!(store.terminal().unwrap(), expected);
    let second = run().unwrap_err();
    assert_eq!(
        crate::native::failure_kind::classify(&second),
        Some(crate::native::failure_kind::FailureKind::Unsupported)
    );
    assert_eq!(first.to_string(), second.to_string());
    assert_eq!(calls, 0);
    assert_eq!(store.terminal().unwrap(), expected);
    assert!(!store.record(CoreRecord::TerminalClosing).present().unwrap());
    assert_eq!(store.status().unwrap().state, SessionState::Ready);
}

#[test]
fn an_ended_replacement_copy_settles_only_on_retry() {
    let (_root, store) = fixture(SessionState::Ready);
    store.write_terminal(&handle(TerminalKind::Herdr)).unwrap();
    let directory = store.directory().to_owned();
    let replacement = herdr_wire();
    let wire = replacement.clone();
    let error = with_step_at(
        "claiming the terminal handle for close",
        move || {
            let store = Store::open_unchecked(&directory);
            store
                .record(CoreRecord::Terminal)
                .write_json(&wire)
                .unwrap();
            update_status(&directory, SessionState::Failed, None, None).unwrap();
        },
        || attempt(&store),
    )
    .unwrap_err();
    assert!(error.is::<CloseRecordChanged>(), "{error:#}");
    assert_eq!(
        error.to_string(),
        CloseRecordChanged("terminal handle").to_string()
    );
    assert_eq!(
        store.record(CoreRecord::Terminal).json::<Value>().unwrap(),
        replacement
    );
    assert!(!store.record(CoreRecord::TerminalClosing).present().unwrap());
    assert!(!store.record(CoreRecord::TerminalClosed).present().unwrap());
    assert_eq!(store.status().unwrap().state, SessionState::Failed);
    attempt(&store).unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Closed);
    assert!(!store.record(CoreRecord::Terminal).present().unwrap());
}

#[test]
fn cleanup_refuses_an_ended_copy_without_claiming_it_is_running() {
    let origin = if cfg!(windows) { "macOS" } else { "Windows" };
    for birth in [true, false] {
        let (_root, store) = fixture(SessionState::Exited);
        if birth {
            store
                .record(CoreRecord::Terminal)
                .write_json(&herdr_wire())
                .unwrap();
        } else {
            store.write_owner(&foreign_owner(ID)).unwrap();
        }
        let before = snapshot(&store);
        let error = close_for_cause(
            &store,
            None,
            CloseCause::Cleanup,
            budget::close_deadline(),
            |_| panic!("cleanup attested a copy"),
            |_| panic!("cleanup dispatched a copy"),
        )
        .unwrap_err();
        assert_eq!(
            crate::native::failure_kind::classify(&error),
            Some(crate::native::failure_kind::FailureKind::Unsupported)
        );
        assert_eq!(
            error.to_string(),
            format!(
                "This session was recorded on {origin}. Only an explicit close can settle it from its records."
            )
        );
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn only_a_strict_completion_supplies_the_copy_reason() {
    let deep = format!(
        r#"{{"consumed":true,"reason":"marker-token\u0000","extra":{}0{}}}"#,
        "[".repeat(135),
        "]".repeat(135)
    );
    let shallow = deep
        .replace(&"[".repeat(135), &"[".repeat(32))
        .replace(&"]".repeat(135), &"]".repeat(32));
    for (wire, keep) in [
        (r#"{"consumed":false,"reason":"marker-token\u0000"}"#, false),
        (
            r#"{"consumed":false,"consumed":true,"reason":"marker-token\u0000"}"#,
            false,
        ),
        (
            r#"{"consumed":true,"terminal":42,"reason":"marker-token\u0000"}"#,
            false,
        ),
        (&deep, false),
        (&shallow, true),
        (r#"{"consumed":true,"reason":"marker-token\u0000"}"#, true),
    ] {
        let (_root, store) = fixture(SessionState::Failed);
        store.write_owner(&foreign_owner(ID)).unwrap();
        let marker = store.record(CoreRecord::TerminalClosed);
        marker.write_private(wire.as_bytes()).unwrap();
        attempt(&store).unwrap();
        assert_eq!(
            store
                .status()
                .unwrap()
                .error
                .unwrap()
                .contains("marker-token\0"),
            keep
        );
        assert_eq!(marker.bytes().unwrap().unwrap() == wire.as_bytes(), keep);
    }
}

#[test]
fn positional_handles_supply_only_the_kind_and_birth_they_state() {
    for (wire, copied) in [
        (herdr_wire(), true),
        (
            json!(["windows-console", "surface", null, null, ID]),
            !cfg!(windows),
        ),
    ] {
        let (_root, store) = fixture(SessionState::Exited);
        store
            .record(CoreRecord::Terminal)
            .write_json(&wire)
            .unwrap();
        let mut calls = 0;
        close(&store, None, budget::close_deadline(), |_| {
            calls += 1;
            Ok(terminal::CloseOutcome::Missing.into())
        })
        .unwrap();
        assert_eq!(calls, usize::from(!copied));
        assert_eq!(
            store.terminal_closed().unwrap().terminal.unwrap().as_str(),
            wire[0].as_str().unwrap()
        );
    }
}

#[test]
fn copy_recovery_publishes_the_unwritten_journal_event() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(ID);
    let (_, event) = seed_event_written_completion(&dir, "placeholder");
    fs::remove_file(&event).unwrap();
    let store = Store::open_unchecked(&dir);
    update_status(
        &dir,
        SessionState::Failed,
        None,
        Some("launch diagnosis".into()),
    )
    .unwrap();
    store.write_terminal(&handle(foreign_kind())).unwrap();
    attempt(&store).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(event).unwrap()).unwrap()["message"],
        "late result"
    );
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Closed);
    assert!(status.error.unwrap().contains("completion recovery failed"));
    assert!(!store.record(CoreRecord::Completion).present().unwrap());
}

#[test]
fn restored_copy_supersedes_the_unlocked_dead_owner_verdict() {
    use crate::native::session::primitives::close_io_test::{self, Point};
    let (_root, store) = fixture(SessionState::Failed);
    store
        .write_owner(&NativeSessionOwner {
            pid: 42,
            ..Default::default()
        })
        .unwrap();
    let claimed = store.record(CoreRecord::TerminalClosing);
    claimed.write_json(&handle(foreign_kind())).unwrap();
    let from = claimed.path().to_owned();
    let to = store.record(CoreRecord::Terminal).path().to_owned();
    let mut locks = 0;
    let restored = close_io_test::during(
        move |point| {
            if point == Point::BeforeLock("turn claim lifecycle") {
                locks += 1;
                if locks == 2 {
                    fs::rename(&from, &to).unwrap();
                }
            }
        },
        || {
            repair_dead_owner_attested_until(
                &store,
                |_| super::super::owner::OwnerAttestation {
                    process: ProcessObservation::Gone(
                        crate::native::process_identity::Gone::Exited,
                    ),
                    control: super::super::owner::ControlAttestation::NotRecorded,
                    pid_alive: Some(false),
                },
                budget::close_deadline(),
            )
        },
    )
    .unwrap();
    assert!(!restored, "Repair settled from the stale verdict");
    assert!(store.record(CoreRecord::Terminal).present().unwrap());
    assert!(store.closed_if_present().unwrap().is_none());
}

#[test]
fn windows_repair_leaves_the_restored_copy_open() {
    use crate::native::session::primitives::close_io_test::{self, Point};
    for restore in [false, true] {
        let (_root, store) = fixture(SessionState::Working);
        store
            .write_owner(&NativeSessionOwner {
                pid: 42,
                ..Default::default()
            })
            .unwrap();
        // A restored copy must state a foreign kind on this OS.
        let kind = if restore {
            if cfg!(target_os = "macos") {
                TerminalKind::Tmux
            } else {
                TerminalKind::Warp
            }
        } else {
            TerminalKind::WindowsConsole
        };
        let claimed = store.record(CoreRecord::TerminalClosing);
        claimed.write_json(&handle(kind)).unwrap();
        let bytes = claimed.bytes().unwrap().unwrap();
        let from = claimed.path().to_owned();
        let to = store.record(CoreRecord::Terminal).path().to_owned();
        let mut locks = 0;

        let changed = close_io_test::during(
            move |point| {
                if point == Point::BeforeLock("turn claim lifecycle") {
                    locks += 1;
                    if restore && locks == 2 {
                        fs::rename(&from, &to).unwrap();
                    }
                }
            },
            || {
                repair_owner(
                    &store,
                    |_| Ok(false),
                    RepairSurfaces::Windows,
                    budget::close_deadline(),
                    None,
                )
            },
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(store.status().unwrap().state, SessionState::Working);
        assert_eq!(
            store.record(CoreRecord::Terminal).bytes().unwrap(),
            restore.then_some(bytes.clone())
        );
        assert_eq!(claimed.bytes().unwrap(), (!restore).then_some(bytes));
    }
}

#[test]
fn prune_and_repair_classify_marker_owner_and_active_records() {
    for evidence in ["marker", "owner", "active"] {
        let (_root, store) = fixture(SessionState::Exited);
        store
            .write_owner(&NativeSessionOwner {
                pid: std::process::id(),
                ..Default::default()
            })
            .unwrap();
        match evidence {
            "marker" => store
                .write_terminal_closed(&json!({"consumed":true,"terminal":foreign_kind()}))
                .unwrap(),
            "owner" => store.write_owner(&foreign_owner(ID)).unwrap(),
            _ => store.write_terminal(&handle(foreign_kind())).unwrap(),
        }
        assert!(
            !repair_dead_owner_attested_until(
                &store,
                |_| panic!("copy owner observed during Repair"),
                budget::close_deadline()
            )
            .unwrap()
        );
        assert_eq!(store.status().unwrap().state, SessionState::Exited);
        assert!(!store.native_owner_blocks_prune().unwrap());
        super::super::test_support::lifecycle::write_test_manifest(store.directory());
        attempt(&store).unwrap();
        assert_eq!(
            crate::native::prune::prune_closed_sessions(_root.path(), u128::MAX).unwrap(),
            [ID]
        );
    }
}

#[test]
fn copy_launch_uses_only_the_recorded_deadline() {
    for (expired, host) in [
        (false, None),
        (true, None),
        (false, Some("{")),
        (false, Some(r#"{"schema":99}"#)),
    ] {
        let (dir, store, _) = launch::test_support::pending_launch(
            "session-copy-",
            &std::env::temp_dir(),
            Duration::from_secs(60),
        );
        let id = dir.path().file_name().unwrap().to_str().unwrap();
        store.write_owner(&foreign_owner(id)).unwrap();
        if let Some(wire) = host {
            store
                .record(CoreRecord::LaunchHost)
                .write_private(wire.as_bytes())
                .unwrap();
        }
        if expired {
            let mut record = launch::read(&store).unwrap().unwrap();
            record.deadline_unix_ms = 1;
            store
                .record(CoreRecord::Launch)
                .write_json(&record)
                .unwrap();
        }
        let before = snapshot(&store);
        let cleanup = close_for_cause(
            &store,
            None,
            CloseCause::Cleanup,
            budget::close_deadline(),
            |_| panic!("cleanup attested a launching copy"),
            |_| panic!("cleanup dispatched a launching copy"),
        )
        .unwrap_err();
        assert_eq!(snapshot(&store), before);
        if expired {
            let origin = if cfg!(windows) { "macOS" } else { "Windows" };
            assert_eq!(
                cleanup.to_string(),
                format!(
                    "This session was recorded on {origin}. Only an explicit close can settle it from its records."
                )
            );
            attempt(&store).unwrap();
            assert_eq!(store.status().unwrap().state, SessionState::Closed);
        } else {
            let error = attempt(&store).unwrap_err();
            assert_eq!(cleanup.to_string(), error.to_string(), "host: {host:?}");
            assert_eq!(
                crate::native::failure_kind::classify(&error),
                Some(crate::native::failure_kind::FailureKind::Unsupported)
            );
            assert_eq!(snapshot(&store), before);
            repair_dead_owner(&store).unwrap();
            assert_eq!(snapshot(&store), before);
        }
    }
}

#[test]
fn cleanup_and_explicit_close_report_the_same_unreadable_launch_receipt_or_claim() {
    for record in [CoreRecord::Launch, CoreRecord::TurnClaim] {
        let (dir, store, _) = launch::test_support::pending_launch(
            "session-copy-",
            &std::env::temp_dir(),
            Duration::from_secs(60),
        );
        store
            .write_owner(&foreign_owner(
                dir.path().file_name().unwrap().to_str().unwrap(),
            ))
            .unwrap();
        fs::write(store.record(record).path(), [0xff]).unwrap();
        let before = snapshot(&store);
        let read_error = match record {
            CoreRecord::Launch => launch::read(&store).unwrap_err(),
            CoreRecord::TurnClaim => super::super::turn::current_claim_token(&store).unwrap_err(),
            _ => unreachable!(),
        };
        for cause in [CloseCause::Explicit, CloseCause::Cleanup] {
            let error = close_for_cause(
                &store,
                None,
                cause,
                budget::close_deadline(),
                |_| panic!("copy owner observed"),
                |_| panic!("copy dispatched"),
            )
            .unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                format!("{read_error:#}"),
                "{} {cause:?}",
                store.record(record).path().display()
            );
            assert_eq!(snapshot(&store), before);
        }
    }
}

#[test]
fn status_timeout_preserves_the_last_herdr_birth_on_a_copy() {
    use crate::native::session::primitives::close_io_test::{self, Point};
    let mut outcomes = Vec::new();
    for pid in [std::process::id(), 0] {
        let (_root, store) = fixture(SessionState::Failed);
        store
            .write_owner(&NativeSessionOwner {
                pid,
                ..Default::default()
            })
            .unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&herdr_wire())
            .unwrap();
        let active = store.record(CoreRecord::Terminal).bytes().unwrap();
        let held = store.lock_status().unwrap();
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let fake = crate::native::clock::FakeClock::new(now);
        let time = fake.shared();
        let scope = budget::Scope::with_clock(fake);
        let result = close_io_test::during(
            move |point| {
                if point == Point::WouldBlock("status") {
                    time.set(deadline);
                }
            },
            || {
                close_with_attestation(
                    &store,
                    None,
                    deadline,
                    |_| panic!("copy owner observed"),
                    |_| panic!("copy dispatched"),
                )
            },
        );
        assert!(result.is_err());
        assert_eq!(store.record(CoreRecord::Terminal).bytes().unwrap(), active);
        let marker = store
            .record(CoreRecord::TerminalClosed)
            .bytes()
            .unwrap()
            .unwrap();
        assert!(store.closed_if_present().unwrap().is_none());
        drop(scope);
        drop(held);
        attempt(&store).unwrap();
        let status = store.status().unwrap();
        outcomes.push((status.state, status.generation, status.error, marker));
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

#[test]
fn a_copy_replays_a_tombstone_and_partial_handle_removal() {
    for obstruction in [false, true] {
        let (_root, store) = fixture(SessionState::Failed);
        store
            .record(CoreRecord::Terminal)
            .write_json(&herdr_wire())
            .unwrap();
        store
            .record(CoreRecord::TerminalClosing)
            .write_json(&handle(TerminalKind::Herdr))
            .unwrap();
        let intent = store
            .directory()
            .join(super::super::names::TERMINAL_CLOSE_INTENT_FILE);
        if obstruction {
            fs::create_dir(&intent).unwrap();
            assert!(attempt(&store).is_err());
        } else {
            assert!(
                interruption::during(interruption::Point::ClosedTombstoneWritten, || attempt(
                    &store
                ))
                .is_err()
            );
        }
        assert!(store.record(CoreRecord::Terminal).present().unwrap());
        assert_eq!(
            store.record(CoreRecord::TerminalClosing).present().unwrap(),
            !obstruction
        );
        let closed = store.record(CoreRecord::Closed).bytes().unwrap().unwrap();
        let marker = store
            .record(CoreRecord::TerminalClosed)
            .bytes()
            .unwrap()
            .unwrap();
        if obstruction {
            fs::remove_dir(&intent).unwrap();
        }
        attempt(&store).unwrap();
        assert_eq!(
            store.record(CoreRecord::Closed).bytes().unwrap().unwrap(),
            closed
        );
        assert_eq!(
            store
                .record(CoreRecord::TerminalClosed)
                .bytes()
                .unwrap()
                .unwrap(),
            marker
        );
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert!(!store.record(CoreRecord::Terminal).present().unwrap());
        assert!(!store.record(CoreRecord::TerminalClosing).present().unwrap());
    }
}

#[test]
fn only_positive_bound_evidence_refuses_a_running_session() {
    let (_root, store) = fixture(SessionState::Ready);
    let foreign = foreign_kind();
    let active = store.record(CoreRecord::Terminal);
    let marker = store.record(CoreRecord::TerminalClosed);
    let owner = store.record(CoreRecord::Owner);
    let lookup = || {
        let records = SurfaceRecords::read(&store);
        records
            .evidence(&store, &super::super::owner::observe(&store))
            .map(|copy| copy.running_refusal().to_string())
    };
    for value in [
        json!({"session_id":"s","managed_session_id":ID}),
        json!({"terminal":foreign,"session_id":"s"}),
        json!({"terminal":foreign,"session_id":"","managed_session_id":ID}),
        json!({"terminal":foreign,"session_id":"s","managed_session_id":"other"}),
    ] {
        active.write_json(&value).unwrap();
        assert_eq!(lookup(), None);
    }
    active.remove().unwrap();
    store
        .record(CoreRecord::TerminalClosing)
        .write_json(&handle(foreign))
        .unwrap();
    assert_eq!(lookup(), None);
    store.write_owner(&foreign_owner("other")).unwrap();
    assert_eq!(lookup(), None);
    owner.remove().unwrap();
    for wire in [
        format!(
            r#"{{"consumed":true,"terminal":"{}","terminal":"{}"}}"#,
            foreign.as_str(),
            foreign.as_str()
        ),
        format!(
            r#"{{"consumed":true,"consumed":true,"terminal":"{}"}}"#,
            foreign.as_str()
        ),
        format!(r#"{{"consumed":false,"terminal":"{}"}}"#, foreign.as_str()),
    ] {
        fs::write(marker.path(), wire).unwrap();
        assert_eq!(lookup(), None);
    }
    marker.remove().unwrap();
    let mut mixed = foreign_owner(ID);
    mixed.process_start_seconds = Some(1);
    mixed.windows_process_identity = Some(terminal::WindowsProcessIdentity {
        creation_time: 7,
        executable_path: "program".into(),
    });
    store.write_owner(&mixed).unwrap();
    let current = if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    };
    assert_eq!(
        lookup().unwrap(),
        format!(
            "This session was recorded on multiple operating systems and cannot be closed on {current} while its records leave it running."
        )
    );
}

#[test]
fn a_copy_keeps_its_handles_when_the_tombstone_does_not_commit_closed() {
    let (_root, store) = fixture(SessionState::Failed);
    store.write_terminal(&handle(foreign_kind())).unwrap();
    store
        .record(CoreRecord::Closed)
        .write_json(&store.status().unwrap())
        .unwrap();
    let before = snapshot(&store);
    assert!(attempt(&store).is_err());
    assert_eq!(before, snapshot(&store));
}
