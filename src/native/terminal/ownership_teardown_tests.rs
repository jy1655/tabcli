// Included inside ownership so all injected capabilities remain private.
use super::*;
use crate::native::tests::write_attested_apple_terminal_state;
use std::cell::Cell;

pub(super) fn fixture(
    kind: terminal::TerminalKind,
) -> (
    tempfile::TempDir,
    NativeSessionOwner,
    TerminalSession,
    NativeProcessIdentity,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = write_attested_apple_terminal_state(dir.path(), "ready", 4242);
    let store = Store::open_unchecked(dir.path());
    owner.terminal_shell = Some(MacTerminalShellIdentity {
        pid: 4000,
        process_group: 4000,
        terminal_tty_device: 7,
        process_start_seconds: 1,
        process_start_microseconds: 0,
    });
    store.record(CoreRecord::Owner).write_json(&owner).unwrap();
    let mut surface: TerminalSession = store.terminal().unwrap();
    surface.kind = kind;
    store
        .record(CoreRecord::Terminal)
        .write_json(&surface)
        .unwrap();
    let live = NativeProcessIdentity {
        pid: owner.pid,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: owner.process_start_seconds.unwrap(),
        process_start_microseconds: owner.process_start_microseconds.unwrap(),
    };
    (dir, owner, surface, live)
}

pub(super) fn kinds() -> [terminal::TerminalKind; 5] {
    use terminal::TerminalKind::*;
    [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal]
}

#[test]
fn r3_teardown_records_intent_before_signal_and_settles_every_kind() {
    for kind in kinds() {
        let (dir, owner, surface, live) = fixture(kind);
        let store = Store::open_unchecked(dir.path());
        let attestations = Cell::new(0);
        let outcome = session::close::compatibility::close_with_result(&store, None, |_| {
            assert_eq!(
                decision(dir.path(), &surface, true, true, |_| panic!(
                    "no replay group probe"
                ))
                .unwrap(),
                TerminalCloseAuthority::OwnerTeardown
            );
            teardown_absent_owner_with(
                &store,
                "session-owner123",
                &surface,
                || {
                    attestations.set(attestations.get() + 1);
                    Ok((owner.clone(), live))
                },
                |group| {
                    assert_eq!(attestations.get(), 2);
                    assert_eq!(group, 4242);
                    let intent =
                        session::close::terminal_teardown_intent(&store, &surface)?.unwrap();
                    assert_eq!(intent.process_group, group);
                    assert_eq!(intent.reason, OWNER_TEARDOWN_REASON);
                    Ok(())
                },
            )
        })
        .unwrap();
        assert_eq!(outcome, CloseOutcome::Missing);
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
        assert_eq!(
            store.status().unwrap().error.as_deref(),
            Some(OWNER_TEARDOWN_REASON)
        );
        assert!(!store.record(CoreRecord::Terminal).path().exists());
        assert!(!store.record(CoreRecord::TerminalClosing).path().exists());
    }
}

#[test]
fn r3_attestation_changes_and_timeout_keep_handle_and_intent() {
    for failure in ["initial", "died", "reused", "timeout", "EPERM"] {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Iterm2);
        let store = Store::open_unchecked(dir.path());
        let calls = Cell::new(0);
        let signals = Cell::new(0);
        let result = session::close::compatibility::close_with_result(&store, None, |_| {
            teardown_absent_owner_with(
                &store,
                "session-owner123",
                &surface,
                || {
                    calls.set(calls.get() + 1);
                    if failure == "initial" || (failure == "died" && calls.get() == 2) {
                        bail!("owner gone");
                    }
                    let mut observed = owner.clone();
                    let mut process = live;
                    if failure == "reused" && calls.get() == 2 {
                        observed.process_start_microseconds = Some(43);
                        process.process_start_microseconds = 43;
                    }
                    Ok((observed, process))
                },
                |_| {
                    signals.set(signals.get() + 1);
                    bail!("group disappearance unconfirmed: {failure}")
                },
            )
        });
        assert!(result.is_err());
        assert_eq!(
            signals.get(),
            usize::from(matches!(failure, "timeout" | "EPERM"))
        );
        assert_eq!(
            store
                .record(CoreRecord::TerminalCloseIntent)
                .path()
                .exists(),
            failure != "initial"
        );
        assert!(store.record(CoreRecord::Terminal).path().exists());
        assert_ne!(store.status().unwrap().state, SessionState::Closed);
    }
}

#[test]
fn r3_teardown_helper_retries_after_reattest_error() {
    let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Iterm2);
    let store = Store::open_unchecked(dir.path());
    let calls = Cell::new(0);
    assert!(
        teardown_absent_owner_with(
            &store,
            "session-owner123",
            &surface,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    bail!("interrupted before signal")
                }
                Ok((owner.clone(), live))
            },
            |_| panic!("no signal")
        )
        .is_err()
    );
    let signals = Cell::new(0);
    teardown_absent_owner_with(
        &store,
        "session-owner123",
        &surface,
        || Ok((owner.clone(), live)),
        |_| {
            signals.set(signals.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(signals.get(), 1);
}

fn decision(
    dir: &Path,
    surface: &TerminalSession,
    live: bool,
    pid_alive: bool,
    group_gone: impl Fn(u32) -> Result<bool>,
) -> Result<TerminalCloseAuthority> {
    verify_terminal_close_authority_with_observers(
        dir,
        "session-owner123",
        surface,
        || Ok(false),
        |_| Ok(None),
        || Ok(vec![]),
        CloseObservers {
            owner_record: &|_| panic!("spawn recovery must not run"),
            pid_alive: &|_| pid_alive,
            mac_owner_live: &|_| Ok(live),
            resumable: &|| {
                session::close::terminal_close_resumable(
                    &Reader::open_unchecked(dir),
                    surface,
                    terminal::surface_outlives_owner(surface),
                )
            },
            ownership_proof: &|| bail!("restored iTerm2: did not match exactly one session"),
            attest_owner: &|| Ok(()),
            group_gone: &group_gone,
        },
    )
}

#[test]
fn r3_live_intent_obtains_fresh_authority_and_reused_pid_refuses() {
    for kind in kinds() {
        let (dir, owner, surface, _) = fixture(kind);
        let store = Store::open_unchecked(dir.path());
        session::close::record_terminal_teardown_intent(
            &store,
            "session-owner123",
            &surface,
            &owner,
            4242,
            OWNER_TEARDOWN_REASON,
        )
        .unwrap();
        assert_eq!(
            decision(dir.path(), &surface, true, true, |_| panic!(
                "live owner must obtain fresh authority"
            ))
            .unwrap(),
            TerminalCloseAuthority::OwnerTeardown
        );
        assert!(
            decision(dir.path(), &surface, false, true, |_| panic!(
                "reused pid grants nothing"
            ))
            .unwrap_err()
            .to_string()
            .contains("PID is reused")
        );
    }
}

#[test]
fn r3_terminal_incarnation_changes_or_old_window_in_new_app_refuse() {
    for case in ["reused", "multiple", "restart-during", "old-id-in-new-app"] {
        let (dir, mut owner, surface, _) = fixture(terminal::TerminalKind::AppleTerminal);
        let store = Store::open_unchecked(dir.path());
        let app = owner.terminal_app.clone().unwrap();
        if case == "old-id-in-new-app" {
            owner.terminal_app = None;
            store.record(CoreRecord::Owner).write_json(&owner).unwrap();
        }
        let births = Cell::new(0);
        let result = verify_terminal_close_authority_with_observers(
            dir.path(),
            "session-owner123",
            &surface,
            || Ok(case == "old-id-in-new-app"),
            |_| {
                births.set(births.get() + 1);
                Ok(Some((
                    app.start_seconds,
                    app.start_microseconds
                        + u64::from(
                            case == "reused" || (case == "restart-during" && births.get() == 3),
                        ),
                )))
            },
            || {
                Ok(if case == "multiple" {
                    vec![
                        app.clone(),
                        MacTerminalAppIdentity {
                            pid: app.pid + 1,
                            ..app.clone()
                        },
                    ]
                } else {
                    vec![app.clone()]
                })
            },
            CloseObservers {
                owner_record: &|_| panic!("not startup"),
                pid_alive: &|_| true,
                mac_owner_live: &|_| Ok(true),
                resumable: &|| Ok(false),
                ownership_proof: &|| bail!("original proof mismatch"),
                attest_owner: &|| panic!("unproven absence must not attest or signal"),
                group_gone: &|_| panic!("not a replay"),
            },
        );
        let error = result.unwrap_err().to_string();
        assert!(
            if case == "reused" {
                error.contains("PID was reused")
            } else if case == "multiple" {
                error.contains("ambiguous or changed")
            } else {
                error == "original proof mismatch"
            },
            "{case}: {error}"
        );
        assert!(
            !store
                .record(CoreRecord::TerminalCloseIntent)
                .path()
                .exists()
        );
        assert!(store.record(CoreRecord::Terminal).path().exists());
    }
}

#[test]
fn r3_owner_dying_during_live_intent_retry_cannot_bypass_group_check() {
    let (dir, owner, surface, _) = fixture(terminal::TerminalKind::Iterm2);
    let store = Store::open_unchecked(dir.path());
    session::close::record_terminal_teardown_intent(
        &store,
        "session-owner123",
        &surface,
        &owner,
        4242,
        OWNER_TEARDOWN_REASON,
    )
    .unwrap();
    let live_reads = Cell::new(0);
    let result = verify_terminal_close_authority_with_observers(
        dir.path(),
        "session-owner123",
        &surface,
        || Ok(false),
        |_| panic!("not Terminal.app"),
        || panic!("not Terminal.app"),
        CloseObservers {
            owner_record: &|_| panic!("not startup"),
            pid_alive: &|_| false,
            mac_owner_live: &|_| {
                live_reads.set(live_reads.get() + 1);
                Ok(live_reads.get() == 1)
            },
            resumable: &|| Ok(false),
            ownership_proof: &|| bail!("owner ended during retry"),
            attest_owner: &|| bail!("owner is now dead"),
            group_gone: &|_| Ok(false),
        },
    );
    assert!(
        result.is_err(),
        "a live-intent retry must not become ordinary absence: {result:?}"
    );
}

#[test]
fn r3_teardown_intent_cannot_fall_through_to_ownerless_startup_authority() {
    let (dir, owner, surface, _) = fixture(terminal::TerminalKind::Iterm2);
    let store = Store::open_unchecked(dir.path());
    session::close::record_terminal_teardown_intent(
        &store,
        "session-owner123",
        &surface,
        &owner,
        4242,
        OWNER_TEARDOWN_REASON,
    )
    .unwrap();
    let mut status = store.status().unwrap();
    status.state = SessionState::Failed;
    store
        .record(CoreRecord::Status)
        .write_json(&status)
        .unwrap();
    std::fs::remove_file(store.record(CoreRecord::Owner).path()).unwrap();
    let result = decision(dir.path(), &surface, false, false, |_| Ok(false));
    assert!(result.is_err(), "missing owner record granted {result:?}");
}

fn damaged_teardown_refuses(path: &str) {
    for payload in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!("damaged"),
    ] {
        let kind = if path == "ordinary" {
            terminal::TerminalKind::Ghostty
        } else {
            terminal::TerminalKind::Iterm2
        };
        let (dir, owner, surface, _) = fixture(kind);
        let store = Store::open_unchecked(dir.path());
        session::close::record_terminal_teardown_intent(
            &store,
            "session-owner123",
            &surface,
            &owner,
            4242,
            OWNER_TEARDOWN_REASON,
        )
        .unwrap();
        let record = store.record(CoreRecord::TerminalCloseIntent);
        let mut intent: serde_json::Value = record.json().unwrap();
        intent["teardown"] = payload.clone();
        record.write_json(&intent).unwrap();
        if path == "failed-start" {
            let mut status = store.status().unwrap();
            status.state = SessionState::Failed;
            store
                .record(CoreRecord::Status)
                .write_json(&status)
                .unwrap();
            store
                .record(CoreRecord::Launch)
                .write_json(&launch::Record {
                    schema: 1,
                    claim_token: "claim-damaged".into(),
                    deadline_unix_ms: 1,
                    phase: launch::Phase::Spawning,
                })
                .unwrap();
        }
        let before = record.bytes().unwrap();
        let result = verify_terminal_close_authority_with_observers(
            dir.path(),
            "session-owner123",
            &surface,
            || Ok(false),
            |_| panic!("not Terminal.app"),
            || panic!("not Terminal.app"),
            CloseObservers {
                owner_record: &|_| session::OwnerObservation {
                    process_alive: Some(false),
                    identity_matches: Some(false),
                    error: None,
                },
                pid_alive: &|_| false,
                mac_owner_live: &|_| Ok(false),
                resumable: &|| {
                    Ok(session::close::terminal_close_intent_owner(
                        &store,
                        &surface,
                        terminal::surface_outlives_owner(&surface),
                    )?
                    .is_some())
                },
                ownership_proof: &|| panic!("dead owner"),
                attest_owner: &|| panic!("dead owner"),
                group_gone: &|_| Ok(false),
            },
        );
        assert!(result.is_err(), "{path}, {payload}: granted {result:?}");
        assert_eq!(record.bytes().unwrap(), before);
        assert!(store.record(CoreRecord::Terminal).path().exists());
        assert_ne!(store.status().unwrap().state, SessionState::Closed);
    }
}

#[test]
fn r3_damaged_teardown_ordinary_replay_refuses() {
    damaged_teardown_refuses("ordinary");
}
#[test]
fn r3_damaged_teardown_iterm_dead_owner_refuses() {
    damaged_teardown_refuses("iterm");
}
#[test]
fn r3_damaged_teardown_failed_start_refuses() {
    damaged_teardown_refuses("failed-start");
}

#[test]
fn r3_damaged_teardown_never_reads_as_legacy_intent() {
    let (dir, owner, surface, _) = fixture(terminal::TerminalKind::Ghostty);
    let store = Store::open_unchecked(dir.path());
    session::close::record_terminal_close_intent(&store, "session-owner123", &surface, &owner)
        .unwrap();
    // Only a missing field retains legacy compatibility.
    assert!(
        session::close::terminal_close_intent_owner(&store, &surface, true)
            .unwrap()
            .is_some()
    );
    for payload in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!("damaged"),
    ] {
        let record = store.record(CoreRecord::TerminalCloseIntent);
        let mut intent: serde_json::Value = record.json().unwrap();
        intent["teardown"] = payload.clone();
        record.write_json(&intent).unwrap();
        let ordinary = session::close::terminal_close_intent_owner(&store, &surface, true);
        assert!(
            !matches!(ordinary, Ok(Some(_))),
            "{payload}: read as legacy"
        );
        assert!(session::close::terminal_teardown_intent(&store, &surface).is_err());
        assert!(!matches!(
            session::close::terminal_close_resumable(&store, &surface, true),
            Ok(true)
        ));
    }
}
