use crate::native::session::close::budget::close_remaining;
// Included inside ownership so all injected capabilities remain private. The decision
// and the dispatch are driven through the in-memory observation: its Records are read
// for real at the decision's own access points, and only the OS and surface
// observations are injected.
use super::*;
use crate::native::session::{CoreRecord, SessionState, launch};
use crate::native::terminal::close_authority::test_support::TestObservation;
use crate::native::terminal::close_authority::{TerminalCloseAuthority, close_owned_surface_with};
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
        let outcome = session::close::close_with_attestation(
            &store,
            None,
            session::close::budget::close_deadline(),
            |_| test_owner_attestation(true),
            |_| {
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
                    crate::native::session::close::budget::close_deadline(),
                )
            },
        )
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
        let result = session::close::close_with_attestation(
            &store,
            None,
            session::close::budget::close_deadline(),
            |_| test_owner_attestation(true),
            |_| {
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
                    crate::native::session::close::budget::close_deadline(),
                )
            },
        );
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
            |_| panic!("no signal"),
            crate::native::session::close::budget::close_deadline()
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
        crate::native::session::close::budget::close_deadline(),
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
    let deadline = crate::native::session::close::budget::close_deadline();
    let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
        |_| Ok(None),
        || Ok(vec![]),
    );
    TestObservation::on_records(dir, "session-owner123", surface, deadline)
        .with_presence(|| Ok(false))
        .with_evidence(if surface.kind == terminal::TerminalKind::AppleTerminal {
            &apple
        } else {
            terminal::close_evidence(surface.kind)
        })
        .with_owner_record(|_| panic!("spawn recovery must not run"))
        .with_pid_alive(move |_| pid_alive)
        .with_mac_owner_live(move |_| test_owner_attestation(live))
        .with_proof(|| bail!("restored iTerm2: did not match exactly one session"))
        .with_attest(|| Ok(()))
        .with_group_gone(group_gone)
        .decide("session-owner123", surface, deadline)
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
        let deadline = crate::native::session::close::budget::close_deadline();
        let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
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
        );
        let result =
            TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
                .with_presence(|| Ok(case == "old-id-in-new-app"))
                .with_evidence(&apple)
                .with_owner_record(|_| panic!("not startup"))
                .with_pid_alive(|_| true)
                .with_mac_owner_live(|_| test_owner_attestation(true))
                .with_resumable(|| Ok(false))
                .with_proof(|| bail!("original proof mismatch"))
                .with_attest(|| panic!("unproven absence must not attest or signal"))
                .with_group_gone(|_| panic!("not a replay"))
                .decide("session-owner123", &surface, deadline);
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
    let deadline = crate::native::session::close::budget::close_deadline();
    let result = TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
        .with_presence(|| Ok(false))
        .with_evidence(terminal::close_evidence(surface.kind))
        .with_owner_record(|_| panic!("not startup"))
        .with_pid_alive(|_| false)
        .with_mac_owner_live(|_| {
            live_reads.set(live_reads.get() + 1);
            test_owner_attestation(live_reads.get() == 1)
        })
        .with_resumable(|| Ok(false))
        .with_proof(|| bail!("owner ended during retry"))
        .with_attest(|| bail!("owner is now dead"))
        .with_group_gone(|_| Ok(false))
        .decide("session-owner123", &surface, deadline);
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
        let deadline = crate::native::session::close::budget::close_deadline();
        let result =
            TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
                .with_presence(|| Ok(false))
                .with_evidence(terminal::close_evidence(surface.kind))
                .with_owner_record(|_| test_owner_attestation(false))
                .with_pid_alive(|_| false)
                .with_mac_owner_live(|_| test_owner_attestation(false))
                .with_resumable(|| {
                    Ok(session::close::terminal_close_intent_owner(
                        &store,
                        &surface,
                        terminal::surface_outlives_owner(&surface),
                    )?
                    .is_some())
                })
                .with_proof(|| panic!("dead owner"))
                .with_attest(|| panic!("dead owner"))
                .with_group_gone(|_| Ok(false))
                .decide("session-owner123", &surface, deadline);
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

#[test]
fn close_deadline_expired_before_signal_sends_nothing() {
    let signals = Cell::new(0);
    let result = terminate_owned_foreground_group_with_io(
        4242,
        Duration::ZERO,
        |_, _| {
            signals.set(signals.get() + 1);
            Ok(())
        },
        |_| {},
    );
    assert!(result.is_err());
    assert_eq!(signals.get(), 0);
}

#[test]
fn close_deadline_between_attestations_keeps_handle_and_intent() {
    let (dir, owner, surface, live) = fixture(terminal::TerminalKind::WezTerm);
    let store = Store::open_unchecked(dir.path());
    let deadline = Instant::now() + Duration::from_secs(1);
    let attestations = Cell::new(0);
    let signals = Cell::new(0);
    let result = session::close::close(&store, None, deadline, |_| {
        teardown_absent_owner_with(
            &store,
            "session-owner123",
            &surface,
            || {
                attestations.set(attestations.get() + 1);
                if attestations.get() == 2 {
                    std::thread::sleep(
                        deadline.saturating_duration_since(Instant::now())
                            + Duration::from_millis(1),
                    );
                }
                Ok((owner.clone(), live))
            },
            |_| {
                signals.set(signals.get() + 1);
                Ok(())
            },
            deadline,
        )
    });
    assert!(result.is_err());
    assert_eq!(signals.get(), 0);
    assert!(store.record(CoreRecord::Terminal).path().exists());
    assert!(
        store
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists()
    );
}

#[test]
fn close_deadline_after_authority_does_not_start_mutation() {
    let now = Instant::now();
    let fake = crate::native::clock::FakeClock::new(now);
    let clock = fake.shared();
    let _scope = crate::native::session::close::budget::Scope::with_clock(fake);
    let deadline = now + Duration::from_secs(1);
    let observed = Cell::new(false);
    let result = close_owned_surface_with(
        || {
            observed.set(true);
            clock.set(deadline);
            Ok(TerminalCloseAuthority::LiveOwner)
        },
        || panic!("no teardown"),
        |_| panic!("no adapter after expiry"),
        deadline,
    );
    assert!(observed.get(), "authority must run before expiry");
    assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
}

#[test]
fn close_deadline_shared_observations_policy_adapter_and_retry() {
    for stage in ["proof", "presence", "policy", "adapter"] {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Warp);
        let store = Store::open_unchecked(dir.path());
        // A previous attempt's intent remains evidence, not cached authority.
        session::close::record_terminal_close_intent(&store, "session-owner123", &surface, &owner)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let expire = |at| {
            if stage == at {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
                );
            }
        };
        let proof_calls = Cell::new(0);
        let decide = |until| {
            TestObservation::on_records(dir.path(), "session-owner123", &surface, until)
                .with_presence(|| {
                    expire("presence");
                    Ok(false)
                })
                .with_evidence(terminal::close_evidence(surface.kind))
                .with_owner_record(|_| panic!("not startup"))
                .with_pid_alive(|_| true)
                .with_mac_owner_live(|_| test_owner_attestation(true))
                .with_resumable(|| Ok(false))
                .with_proof(|| {
                    proof_calls.set(proof_calls.get() + 1);
                    expire("proof");
                    if stage == "presence" {
                        bail!("surface proof refused")
                    }
                    Ok(())
                })
                .with_attest(|| Ok(()))
                .with_group_gone(|_| panic!("no dead-owner replay"))
                .decide("session-owner123", &surface, until)
        };
        let result = session::close::close(&store, None, deadline, |_| {
            close_owned_surface_with(
                || decide(deadline),
                || panic!("expired observation grants no teardown"),
                |_| {
                    let shell = NativeProcessIdentity {
                        pid: 4000,
                        process_group: 4000,
                        process_start_seconds: 1,
                        process_start_microseconds: 0,
                        ..live
                    };
                    terminal::macos::warp::prepare_warp_close(
                        dir.path(),
                        "session-owner123",
                        &surface,
                        &owner,
                        &live,
                        &shell,
                        |_| {
                            expire("policy");
                            close_remaining(deadline)?;
                            Ok(())
                        },
                        deadline,
                    )?;
                    close_remaining(deadline)?;
                    expire("adapter");
                    Ok(CloseOutcome::Closed)
                },
                deadline,
            )
        });
        assert!(
            format!("{:#}", result.unwrap_err()).contains("deadline"),
            "{stage}"
        );
        assert!(
            store.record(CoreRecord::Terminal).path().exists(),
            "{stage}"
        );
        assert!(
            store
                .record(CoreRecord::TerminalCloseIntent)
                .path()
                .exists(),
            "{stage}"
        );
        assert_ne!(store.status().unwrap().state, SessionState::Closed);
        let before = proof_calls.get();
        // The next attempt observes again and may safely refuse; it promises no success.
        let retry = session::close::close(
            &store,
            None,
            crate::native::session::close::budget::close_deadline(),
            |_| {
                let _ = decide(crate::native::session::close::budget::close_deadline())?;
                bail!("fresh authority still refuses cleanup")
            },
        );
        assert!(retry.is_err());
        assert_eq!(proof_calls.get(), before + 1);
        assert!(store.record(CoreRecord::Terminal).path().exists());
    }
}

#[test]
fn close_deadline_after_signal_eperm_does_not_prove_exit() {
    let now = Instant::now();
    let fake = crate::native::clock::FakeClock::new(now);
    let clock = fake.shared();
    let _scope = crate::native::session::close::budget::Scope::with_clock(fake);
    let deadline = now + Duration::from_secs(1);
    let signals = Cell::new(0);
    let result = terminate_owned_foreground_group_until_with_io(
        4242,
        deadline,
        |_, signal| {
            if signal == libc::SIGTERM {
                signals.set(signals.get() + 1);
                // Expire only after SIGTERM, independent of host scheduling.
                clock.set(deadline);
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(libc::EPERM))
            }
        },
        |_| {},
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
    assert_eq!(signals.get(), 1);
}

#[test]
fn terminal_owner_second_verification_uses_close_deadline() {
    use crate::native::terminal::macos::automation_testing::{Method, Replay, Reply};
    let (dir, _, surface, _) = fixture(terminal::TerminalKind::AppleTerminal);
    let deadline = Instant::now() + Duration::from_secs(60);
    // The first authority proof answers; the second verification stalls. Both run
    // through the one automation of the closing adapter.
    let automation = Replay::new([
        Reply::Normal(Ok(surface.id.clone())),
        Reply::Normal(Err(anyhow::anyhow!(
            "Terminal.app did not answer before the deadline"
        ))),
    ]);
    let dispatch = terminal::macos::apple_terminal_dispatch(automation.clone());
    let result = (|| {
        terminal::macos::verify_surface(
            &dispatch,
            dir.path(),
            &surface,
            Some(crate::native::session::close::budget::close_remaining(
                deadline,
            )?),
        )?;
        terminal::macos::apple_terminal::close_evidence::terminate_owner(
            &automation,
            dir.path(),
            "session-owner123",
            &surface,
            deadline,
        )
    })();
    assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
    let calls = automation.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            call.method,
            Method::RunUntil,
            "every close verification must use bounded automation"
        );
    }
    assert_eq!(calls[1].deadline, Some(deadline));
    automation.assert_finished();
    let store = Store::open_unchecked(dir.path());
    assert!(store.record(CoreRecord::Terminal).path().exists());
    assert!(
        !store
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists()
    );
}

#[test]
fn owner_control_failure_or_reuse_grants_no_ordinary_surface_authority() {
    use crate::native::process_identity::Attested;
    use crate::native::session::owner::ControlAttestation;
    for control in [
        ControlAttestation::Changed("TTY".into()),
        ControlAttestation::Unknown("no TTY".into()),
    ] {
        let (dir, _, surface, _) = fixture(terminal::TerminalKind::Iterm2);
        let deadline = crate::native::session::close::budget::close_deadline();
        let result =
            TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
                .with_presence(|| panic!("control failure must not probe surface"))
                .with_owner_record(|_| panic!("not startup"))
                .with_pid_alive(|_| true)
                .with_mac_owner_live(|_| OwnerAttestation {
                    process: ProcessObservation::Live(Attested::Incarnation),
                    control: match &control {
                        ControlAttestation::Changed(r) => ControlAttestation::Changed(r.clone()),
                        ControlAttestation::Unknown(r) => ControlAttestation::Unknown(r.clone()),
                        _ => unreachable!(),
                    },
                    pid_alive: Some(true),
                })
                .with_resumable(|| Ok(false))
                .with_proof(|| panic!("failed initial control"))
                .with_attest(|| panic!("failed initial control"))
                .with_group_gone(|_| panic!("not teardown"))
                .decide("session-owner123", &surface, deadline);
        assert!(result.is_err());
    }
    let (dir, _, surface, _) = fixture(terminal::TerminalKind::Iterm2);
    let deadline = crate::native::session::close::budget::close_deadline();
    let result = TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
        .with_presence(|| panic!("reuse grants no absence"))
        .with_owner_record(|_| panic!("not startup"))
        .with_pid_alive(|_| false)
        .with_mac_owner_live(|_| OwnerAttestation {
            process: ProcessObservation::Gone(Gone::Reused("birth".into())),
            control: ControlAttestation::NotRecorded,
            pid_alive: Some(false),
        })
        .with_resumable(|| Ok(false))
        .with_proof(|| panic!("reused"))
        .with_attest(|| panic!("reused"))
        .with_group_gone(|_| panic!("reused"))
        .decide("session-owner123", &surface, deadline);
    assert!(result.is_err());
}

#[test]
fn r1_c8_reused_birth_resumes_only_through_exact_intent_at_authority_seam() {
    use crate::native::session::owner::{OwnerObservers, attest_owner_with};
    for intent in [false, true] {
        let (dir, owner, surface, _) = fixture(terminal::TerminalKind::Iterm2);
        let store = Store::open_unchecked(dir.path());
        if intent {
            session::close::record_terminal_close_intent(
                &store,
                "session-owner123",
                &surface,
                &owner,
            )
            .unwrap();
        }
        let observe = |owner: &NativeSessionOwner| {
            attest_owner_with(
                owner,
                OwnerObservers {
                    pid_alive: &|_| true,
                    birth: &|_| {
                        Ok(Some((
                            owner.process_start_seconds.unwrap() + 1,
                            owner.process_start_microseconds.unwrap(),
                        )))
                    },
                    control: &|_| panic!("birth reuse has no control lookup"),
                },
            )
        };
        let deadline = crate::native::session::close::budget::close_deadline();
        let result =
            TestObservation::on_records(dir.path(), "session-owner123", &surface, deadline)
                .with_presence(|| panic!("no surface observation granted"))
                .with_owner_record(|_| panic!("not startup"))
                .with_pid_alive(|_| true)
                .with_mac_owner_live(observe)
                .with_resumable(|| {
                    session::close::terminal_close_resumable_with(&store, &surface, true, observe)
                })
                .with_proof(|| panic!("not live owner"))
                .with_attest(|| panic!("not live owner"))
                .with_group_gone(|_| panic!("not teardown"))
                .decide("session-owner123", &surface, deadline);
        if intent {
            assert_eq!(result.unwrap(), TerminalCloseAuthority::SurfaceOnly);
        } else {
            assert!(result.is_err());
        }
    }
}
