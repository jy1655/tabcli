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
    owner.terminal_shell = Some(TerminalShellIdentity {
        linux_birth: None,
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
        birth: crate::native::terminal::ownership::ProcessBirth::Macos(
            owner.process_start_seconds.unwrap(),
            owner.process_start_microseconds.unwrap(),
        ),
        pid: owner.pid,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
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
                            process.birth = crate::native::terminal::ownership::ProcessBirth::Macos(
                                process.birth.macos().unwrap().0,
                                43,
                            );
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
        .with_posix_owner_live(move |_| test_owner_attestation(live))
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
                .with_posix_owner_live(|_| test_owner_attestation(true))
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
        .with_posix_owner_live(|_| {
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
                .with_posix_owner_live(|_| test_owner_attestation(false))
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
                .with_posix_owner_live(|_| test_owner_attestation(true))
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
                        birth: crate::native::terminal::ownership::ProcessBirth::Macos(1, 0),
                        pid: 4000,
                        process_group: 4000,
                        ..live
                    };
                    stop_attested_owner(
                        &store,
                        "session-owner123",
                        &surface,
                        OwnerStopIntent::Close,
                        OwnerStop {
                            attest: || Ok((owner.clone(), live)),
                            shell: FreshShell(|_| Ok(shell)),
                            record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
                            stop: |_: OwnerGroups<u32>| {
                                expire("policy");
                                close_remaining(deadline)?;
                                Ok(())
                            },
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
                .with_posix_owner_live(|_| OwnerAttestation {
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
        .with_posix_owner_live(|_| OwnerAttestation {
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
                .with_posix_owner_live(observe)
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

// The teardown checks the close budget at its start, after the first attestation, after the
// intent, after the second attestation, before the stop and after it. An expiry at each
// check leaves exactly the effects before it.
#[test]
fn teardown_deadline_expiry_at_each_check() {
    for unexpired_reads in 0..=6 {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Warp);
        let store = Store::open_unchecked(dir.path());
        let start = Instant::now();
        let deadline = start + Duration::from_secs(60);
        let clock = crate::native::clock::ExpiresAtRead::new(start, deadline, unexpired_reads);
        let reads = clock.reads();
        let _scope = crate::native::session::close::budget::Scope::with_clock(clock);
        let (attestations, stops) = (Cell::new(0), Cell::new(0));
        let result = teardown_absent_owner_with(
            &store,
            "session-owner123",
            &surface,
            || {
                attestations.set(attestations.get() + 1);
                Ok((owner.clone(), live))
            },
            |group| {
                assert_eq!(group, 4242);
                stops.set(stops.get() + 1);
                Ok(())
            },
            deadline,
        );
        let intent = session::close::terminal_teardown_intent(&store, &surface)
            .unwrap()
            .is_some();
        // (expired, attestations, intent recorded, stops)
        let expected = match unexpired_reads {
            0 => (true, 0, false, 0),
            1 => (true, 1, false, 0),
            2 => (true, 1, true, 0),
            3 | 4 => (true, 2, true, 0),
            5 => (true, 2, true, 1),
            _ => (false, 2, true, 1),
        };
        let expired = match &result {
            Ok(_) => false,
            Err(error) => {
                assert!(
                    error.to_string().contains("close deadline expired"),
                    "{unexpired_reads}: {error:#}"
                );
                true
            }
        };
        assert_eq!(
            (expired, attestations.get(), intent, stops.get()),
            expected,
            "{unexpired_reads}"
        );
        assert_eq!(
            reads.get(),
            (unexpired_reads + 1).min(6),
            "{unexpired_reads}"
        );
    }
}

// A Terminal.app live-owner close checks the budget in the adapter, before its owner stop
// and before the surface verification of the stop: an expiry at any of the three sends no
// automation and records nothing. With the budget left, the surface is verified first and
// the owner is attested after it.
#[test]
fn terminal_app_live_owner_close_checks_budget_and_surface_before_the_owner() {
    use crate::native::terminal::macos::automation_testing::{Method, Replay, Reply};
    for unexpired_reads in 0..=3 {
        let (dir, _, surface, _) = fixture(terminal::TerminalKind::AppleTerminal);
        std::fs::remove_file(
            Store::open_unchecked(dir.path())
                .record(CoreRecord::Owner)
                .path(),
        )
        .unwrap();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(60);
        let clock = crate::native::clock::ExpiresAtRead::new(start, deadline, unexpired_reads);
        let reads = clock.reads();
        let _scope = crate::native::session::close::budget::Scope::with_clock(clock);
        let automation = Replay::new(if unexpired_reads == 3 {
            vec![Reply::Normal(Ok(surface.id.clone()))]
        } else {
            vec![]
        });
        let dispatch = terminal::macos::apple_terminal_dispatch(automation.clone());
        let error = terminal::macos::close_owned_session(
            &dispatch,
            dir.path(),
            "session-owner123",
            &surface,
            terminal::macos::OwnedSurfaceClose::LiveOwner,
            deadline,
        )
        .unwrap_err();
        if unexpired_reads == 3 {
            assert_eq!(
                error.to_string(),
                "Terminal.app ownership requires a live native-session owner"
            );
            let calls = automation.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].method, Method::RunUntil);
        } else {
            assert!(
                error.to_string().contains("close deadline expired"),
                "{unexpired_reads}: {error:#}"
            );
            assert!(automation.calls().is_empty());
        }
        assert_eq!(
            reads.get(),
            (unexpired_reads + 1).min(3),
            "{unexpired_reads}"
        );
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
}

// A surface that does not verify refuses the Terminal.app live-owner close before the owner
// is read: no intent, no signal.
#[test]
fn terminal_app_live_owner_close_refuses_an_unverified_surface_before_the_owner() {
    use crate::native::terminal::macos::automation_testing::{Replay, Reply};
    let (dir, _, surface, _) = fixture(terminal::TerminalKind::AppleTerminal);
    // An owner record that cannot be read: reaching it would fail with its own error.
    std::fs::write(
        Store::open_unchecked(dir.path())
            .record(CoreRecord::Owner)
            .path(),
        b"{",
    )
    .unwrap();
    let automation = Replay::new([Reply::Normal(Ok("another-tty".into()))]);
    let dispatch = terminal::macos::apple_terminal_dispatch(automation.clone());
    let error = terminal::macos::close_owned_session(
        &dispatch,
        dir.path(),
        "session-owner123",
        &surface,
        terminal::macos::OwnedSurfaceClose::LiveOwner,
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Agent Bridge Terminal.app owned tab is missing"
    );
    automation.assert_finished();
    assert!(
        !Store::open_unchecked(dir.path())
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists()
    );
}

// The shell of `fixture`: the owner's parent, leading its own group on the owner's TTY.
fn fixture_shell(live: NativeProcessIdentity) -> NativeProcessIdentity {
    NativeProcessIdentity {
        birth: crate::native::terminal::ownership::ProcessBirth::Macos(1, 0),
        pid: 4000,
        parent_pid: 3000,
        process_group: 4000,
        ..live
    }
}

// What the intent record says when the stop runs: none, a close intent, or a teardown
// intent with its group.
fn recorded_intent(store: &Store, surface: &TerminalSession) -> String {
    match session::close::terminal_teardown_intent(store, surface).unwrap() {
        Some(teardown) => format!("teardown intent {}", teardown.process_group),
        None if store
            .record(CoreRecord::TerminalCloseIntent)
            .path()
            .exists() =>
        {
            "close intent".to_owned()
        }
        None => "no intent".to_owned(),
    }
}

// One step for every caller: the proof, the shell check, the record of the incarnation, the
// intent and the stop, in this order. Only the teardown attests again after its intent; the
// intent is published after the incarnation record and before the stop.
#[test]
fn owner_stop_runs_proof_shell_incarnation_intent_and_stop_in_order() {
    for (intent, fresh) in [
        (OwnerStopIntent::Close, false),
        (OwnerStopIntent::Close, true),
        (OwnerStopIntent::Teardown, false),
    ] {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Warp);
        let store = Store::open_unchecked(dir.path());
        let events = std::cell::RefCell::new(Vec::<String>::new());
        let attest = || {
            events.borrow_mut().push("attest".into());
            Ok((owner.clone(), live))
        };
        let record_incarnation = |_: &mut NativeSessionOwner| {
            events.borrow_mut().push(format!(
                "incarnation with {}",
                recorded_intent(&store, &surface)
            ));
            Ok(())
        };
        let result = if fresh {
            stop_attested_owner(
                &store,
                "session-owner123",
                &surface,
                intent,
                OwnerStop {
                    attest,
                    shell: FreshShell(|pid| {
                        events.borrow_mut().push(format!("shell {pid}"));
                        Ok(fixture_shell(live))
                    }),
                    record_incarnation,
                    stop: |groups: OwnerGroups<u32>| {
                        events.borrow_mut().push(format!(
                            "stop {} {} with {}",
                            groups.owner,
                            groups.shell,
                            recorded_intent(&store, &surface)
                        ));
                        Ok(())
                    },
                },
                crate::native::session::close::budget::close_deadline(),
            )
        } else {
            stop_attested_owner(
                &store,
                "session-owner123",
                &surface,
                intent,
                OwnerStop {
                    attest,
                    shell: ProvenShell,
                    record_incarnation,
                    stop: |groups: OwnerGroups<()>| {
                        events.borrow_mut().push(format!(
                            "stop {} with {}",
                            groups.owner,
                            recorded_intent(&store, &surface)
                        ));
                        Ok(())
                    },
                },
                crate::native::session::close::budget::close_deadline(),
            )
        };
        result.unwrap();
        let expected: &[&str] = match (intent, fresh) {
            (OwnerStopIntent::Close, false) => &[
                "attest",
                "incarnation with no intent",
                "stop 4242 with close intent",
            ],
            (OwnerStopIntent::Close, true) => &[
                "attest",
                "shell 4000",
                "incarnation with no intent",
                "stop 4242 4000 with close intent",
            ],
            _ => &[
                "attest",
                "incarnation with no intent",
                "attest",
                "stop 4242 with teardown intent 4242",
            ],
        };
        assert_eq!(*events.borrow(), expected, "{intent:?}, fresh {fresh}");
    }
}

// The intent names the owner as the incarnation record left it.
#[test]
fn owner_stop_intent_names_the_owner_the_incarnation_record_completed() {
    let (dir, mut owner, surface, live) = fixture(terminal::TerminalKind::AppleTerminal);
    let app = owner.terminal_app.take().unwrap();
    let store = Store::open_unchecked(dir.path());
    stop_attested_owner(
        &store,
        "session-owner123",
        &surface,
        OwnerStopIntent::Close,
        OwnerStop {
            attest: || Ok((owner.clone(), live)),
            shell: ProvenShell,
            record_incarnation: |owner: &mut NativeSessionOwner| {
                owner.terminal_app = Some(app.clone());
                Ok(())
            },
            stop: |_: OwnerGroups<()>| Ok(()),
        },
        crate::native::session::close::budget::close_deadline(),
    )
    .unwrap();
    let named = session::close::terminal_close_intent_owner(&store, &surface, true)
        .unwrap()
        .unwrap();
    assert_eq!(named.terminal_app, Some(app));
}

// A shell observed again after the proof that ended, cannot be inspected, changed
// incarnation, or no longer holds the owner's job refuses before the incarnation record,
// the intent and the stop. The shell check names the surface's kind.
#[test]
fn owner_stop_refuses_a_changed_shell_before_incarnation_intent_and_stop() {
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
    ] {
        let name = kind.display_name();
        for (case, expected) in [
            (
                "ended",
                "failed to inspect native-session process 4000".to_owned(),
            ),
            ("uninspectable", "process inspection failed".to_owned()),
            ("incarnation", format!("{name} shell identity changed")),
            (
                "tty",
                format!("{name} shell is attached to a different TTY"),
            ),
            (
                "parent",
                format!("{name} shell no longer owns the native-session process"),
            ),
            (
                "job",
                format!("{name} shell does not own the expected foreground job"),
            ),
        ] {
            let (dir, owner, surface, live) = fixture(kind);
            let store = Store::open_unchecked(dir.path());
            let shell = fixture_shell(live);
            let error = stop_attested_owner(
                &store,
                "session-owner123",
                &surface,
                OwnerStopIntent::Close,
                OwnerStop {
                    attest: || Ok((owner.clone(), live)),
                    shell: FreshShell(|pid| {
                        assert_eq!(pid, 4000);
                        match case {
                            "ended" => Err(std::io::Error::from_raw_os_error(libc::ESRCH))
                                .context("failed to inspect native-session process 4000"),
                            "uninspectable" => bail!("process inspection failed"),
                            "incarnation" => Ok(NativeProcessIdentity {
                                birth: crate::native::terminal::ownership::ProcessBirth::Macos(
                                    2,
                                    shell.birth.macos().unwrap().1,
                                ),
                                ..shell
                            }),
                            "tty" => Ok(NativeProcessIdentity {
                                terminal_tty_device: 8,
                                ..shell
                            }),
                            "parent" => Ok(NativeProcessIdentity { pid: 4001, ..shell }),
                            _ => Ok(NativeProcessIdentity {
                                terminal_process_group: 4000,
                                ..shell
                            }),
                        }
                    }),
                    record_incarnation: |_: &mut NativeSessionOwner| -> Result<()> {
                        panic!("{case}: no incarnation is recorded")
                    },
                    stop: |_: OwnerGroups<u32>| -> Result<()> {
                        panic!("{case}: nothing is stopped")
                    },
                },
                crate::native::session::close::budget::close_deadline(),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), expected, "{kind:?} {case}");
            assert_eq!(recorded_intent(&store, &surface), "no intent", "{case}");
        }
    }
}

// A refused proof or incarnation record publishes no intent and stops nothing.
#[test]
fn owner_stop_refusals_before_the_intent_record_nothing() {
    for case in ["proof", "incarnation"] {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::AppleTerminal);
        let store = Store::open_unchecked(dir.path());
        let error = stop_attested_owner(
            &store,
            "session-owner123",
            &surface,
            OwnerStopIntent::Close,
            OwnerStop {
                attest: || {
                    if case == "proof" {
                        bail!("proof refused")
                    }
                    Ok((owner.clone(), live))
                },
                shell: FreshShell(|_| Ok(fixture_shell(live))),
                record_incarnation: |_: &mut NativeSessionOwner| bail!("incarnation refused"),
                stop: |_: OwnerGroups<u32>| -> Result<()> { panic!("{case}: nothing is stopped") },
            },
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), format!("{case} refused"));
        assert_eq!(recorded_intent(&store, &surface), "no intent", "{case}");
    }
}

// The step checks the budget before the proof and before the intent; a close's stop checks
// it again itself. An expiry at each check leaves exactly the effects before it.
#[test]
fn owner_stop_deadline_expiry_at_each_check() {
    for unexpired_reads in 0..=2 {
        let (dir, owner, surface, live) = fixture(terminal::TerminalKind::Warp);
        let store = Store::open_unchecked(dir.path());
        let start = Instant::now();
        let deadline = start + Duration::from_secs(60);
        let clock = crate::native::clock::ExpiresAtRead::new(start, deadline, unexpired_reads);
        let reads = clock.reads();
        let _scope = crate::native::session::close::budget::Scope::with_clock(clock);
        let (attestations, shells, incarnations, stops) =
            (Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0));
        let result = stop_attested_owner(
            &store,
            "session-owner123",
            &surface,
            OwnerStopIntent::Close,
            OwnerStop {
                attest: || {
                    attestations.set(attestations.get() + 1);
                    Ok((owner.clone(), live))
                },
                shell: FreshShell(|_| {
                    shells.set(shells.get() + 1);
                    Ok(fixture_shell(live))
                }),
                record_incarnation: |_: &mut NativeSessionOwner| {
                    incarnations.set(incarnations.get() + 1);
                    Ok(())
                },
                stop: |_: OwnerGroups<u32>| {
                    stops.set(stops.get() + 1);
                    Ok(())
                },
            },
            deadline,
        );
        let intent = recorded_intent(&store, &surface) == "close intent";
        // (expired, attestations, shell reads, incarnation records, intent, stops)
        let expected = match unexpired_reads {
            0 => (true, 0, 0, 0, false, 0),
            1 => (true, 1, 1, 1, false, 0),
            _ => (false, 1, 1, 1, true, 1),
        };
        let expired = match &result {
            Ok(()) => false,
            Err(error) => {
                assert!(
                    error.to_string().contains("close deadline expired"),
                    "{error:#}"
                );
                true
            }
        };
        assert_eq!(
            (
                expired,
                attestations.get(),
                shells.get(),
                incarnations.get(),
                intent,
                stops.get()
            ),
            expected,
            "{unexpired_reads}"
        );
        assert_eq!(
            reads.get(),
            (unexpired_reads + 1).min(2),
            "{unexpired_reads}"
        );
    }
}

// A process group that cannot be a signal target is refused before any signal, by every
// shared stop that signals a group, whatever the kind: the teardown replay's group probe and
// the foreground-group stop of Warp and the teardown. The negation of a positive `pid_t`
// cannot overflow, so the third refusal of `process_group_signal_target` is not reachable.
#[test]
fn process_group_signal_target_refusals_name_no_terminal_application() {
    let out_of_range = i32::MAX as u32 + 1;
    for (group, expected) in [
        (out_of_range, "terminal process group is out of range"),
        (0, "terminal process group must be positive"),
    ] {
        assert_eq!(
            process_group_signal_target(group).unwrap_err().to_string(),
            expected
        );
        assert_eq!(
            owner_group_gone_with(group, |_, _| panic!("{group}: no group is probed"))
                .unwrap_err()
                .to_string(),
            expected
        );
        assert_eq!(
            terminate_owned_foreground_group_until_with_io(
                group,
                Instant::now() + Duration::from_secs(60),
                |_, _| panic!("{group}: no signal is sent"),
                |_| {},
            )
            .unwrap_err()
            .to_string(),
            expected
        );
    }
    assert_eq!(process_group_signal_target(4242).unwrap(), -4242);
}
