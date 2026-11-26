use super::super::{LinuxBirth, TerminalSession, ownership::*};
use super::*;
use crate::native::{
    process_identity::{Attested, Gone, ProcessObservation},
    session::{owner::*, *},
};
use std::time::{Duration, Instant};
fn birth(ticks: u64) -> LinuxBirth {
    serde_json::from_value(serde_json::json!({"boot_id":"00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks":ticks})).unwrap()
}
fn owner() -> NativeSessionOwner {
    NativeSessionOwner {
        pid: 42,
        linux_birth: Some(birth(10)),
        terminal_tty_device: Some(7),
        process_group: Some(42),
        terminal_process_group: Some(42),
        ..Default::default()
    }
}
fn live() -> NativeProcessIdentity {
    NativeProcessIdentity {
        birth: crate::native::terminal::ownership::ProcessBirth::Linux(birth(10)),
        pid: 42,
        parent_pid: 41,
        terminal_tty_device: 7,
        process_group: 42,
        terminal_process_group: 42,
    }
}
#[test]
fn linux_birth_wire_is_strict_and_legacy_fields_keep_their_meaning() {
    assert_eq!(birth(0).starttime_ticks, 0); // Zero is a valid boot-relative tick.
    let owner = owner();
    let value = serde_json::to_value(&owner).unwrap();
    assert_eq!(value["linux_birth"]["starttime_ticks"], 10);
    assert!(value.get("process_start_seconds").is_none());
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"boot_id":"bad", "starttime_ticks":10}),
        serde_json::json!({"boot_id":"00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks":-1}),
    ] {
        let mut value = value.clone();
        value["linux_birth"] = invalid;
        assert!(serde_json::from_value::<NativeSessionOwner>(value).is_err());
    }
    let old: NativeSessionOwner = serde_json::from_value(
        serde_json::json!({"pid":42,"process_start_seconds":100,"process_start_microseconds":12}),
    )
    .unwrap();
    assert_eq!(old.process_start_seconds, Some(100));
    let attestation = attest_owner_with(
        &old,
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| panic!("foreign birth must not be observed"),
            control: &|_| panic!("foreign control"),
        },
    );
    assert!(matches!(
        attestation.process,
        ProcessObservation::Unknown(_)
    ));
}
#[test]
fn linux_attestation_distinguishes_reuse_unknown_partial_control_and_later_reuse() {
    for (observed, expected) in [
        (Some(birth(10)), "live"),
        (Some(birth(11)), "gone"),
        (None, "gone"),
    ] {
        let a = attest_owner_with(
            &owner(),
            OwnerObservers {
                pid_alive: &|_| true,
                birth: &|_| Ok(observed.map(ProcessBirth::Linux)),
                control: &|_| Ok(Some(live())),
            },
        );
        assert_eq!(
            matches!(a.process, ProcessObservation::Live(Attested::Incarnation)),
            expected == "live"
        );
    }
    let a = attest_owner_with(
        &owner(),
        OwnerObservers {
            pid_alive: &|_| false,
            birth: &|_| anyhow::bail!("denied"),
            control: &|_| panic!(),
        },
    );
    assert!(matches!(a.process, ProcessObservation::Unknown(_)));
    let mut changed = live();
    changed.terminal_tty_device = 8;
    let a = attest_owner_with(
        &owner(),
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some(ProcessBirth::Linux(birth(10)))),
            control: &|_| Ok(Some(changed)),
        },
    );
    assert!(matches!(a.control, ControlAttestation::Changed(_)));
    changed.birth = ProcessBirth::Linux(birth(11));
    let a = attest_owner_with(
        &owner(),
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some(ProcessBirth::Linux(birth(10)))),
            control: &|_| Ok(Some(changed)),
        },
    );
    assert!(matches!(
        a.process,
        ProcessObservation::Gone(Gone::Reused(_))
    ));
    let mut partial = owner();
    partial.process_group = None;
    assert!(!partial.control_record().is_complete());
}
#[test]
fn creation_proven_host_close_is_independent_of_owner_and_intents() {
    use super::super::close_authority::{
        TerminalCloseAuthority as A,
        test_support::{RecordValues, TestObservation},
    };
    let surface:TerminalSession=serde_json::from_value(serde_json::json!({"terminal":"iterm2","session_id":"test","managed_session_id":"session-test"})).unwrap();
    for state in [
        SessionState::Ready,
        SessionState::Failed,
        SessionState::Closed,
    ] {
        for intent in ["missing", "mismatched", "interrupted"] {
            for outcome in ["live", "exit", "reuse", "reboot", "unknown", "pid-only"] {
                let obs = TestObservation::from_values(RecordValues {
                    owner: Some(owner()),
                    state: state.clone(),
                    launch: None,
                })
                .with_teardown_intent(|| panic!("{intent}: independent host must not read intent"))
                .with_owner_present(|| panic!("independent host must not read owner"))
                .with_created_host(|| {
                    Ok(Some(match outcome {
                        "live" => ProcessObservation::Live(Attested::Incarnation),
                        "exit" => ProcessObservation::Gone(Gone::Exited),
                        "reuse" | "reboot" => {
                            ProcessObservation::Gone(Gone::Reused(outcome.into()))
                        }
                        "pid-only" => ProcessObservation::Live(Attested::PidOnly),
                        _ => ProcessObservation::Unknown("denied".into()),
                    }))
                });
                let result = obs.decide(
                    "session-test",
                    &surface,
                    Instant::now() + Duration::from_secs(1),
                );
                match outcome {
                    "live" => assert_eq!(result.unwrap(), A::SurfaceOnly),
                    "exit" | "reuse" | "reboot" => assert_eq!(result.unwrap(), A::Absent),
                    _ => assert!(result.is_err()),
                }
                assert!(obs.calls().is_empty());
            }
        }
    }
    let obs = TestObservation::from_values(RecordValues {
        owner: None,
        state: SessionState::Ready,
        launch: None,
    })
    .with_created_host(|| Ok(Some(ProcessObservation::Live(Attested::Incarnation))));
    assert!(
        obs.decide(
            "session-foreign",
            &surface,
            Instant::now() + Duration::from_secs(1)
        )
        .is_err()
    );
}
#[test]
fn linux_registers_tmux_and_herdr() {
    use super::super::TerminalKind;
    use crate::native::failure_kind::{FailureKind, classify};
    assert_eq!(
        select(Some(TerminalKind::Herdr)).unwrap(),
        TerminalKind::Herdr
    );
    assert!(INSTALLED.adapter(TerminalKind::Herdr).is_ok());
    assert_eq!(
        select(Some(TerminalKind::Tmux)).unwrap(),
        TerminalKind::Tmux
    );
    // Without a request Linux chooses tmux; inside hc that is the fallback of the automatic
    // Herdr choice (`terminal::select_for`).
    assert_eq!(select(None).unwrap(), TerminalKind::Tmux);
    assert!(INSTALLED.adapter(TerminalKind::Tmux).is_ok());
    for kind in TerminalKind::ALL {
        if matches!(kind, TerminalKind::Tmux | TerminalKind::Herdr) {
            continue;
        }
        let error = select(Some(kind)).unwrap_err();
        assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{kind:?}");
        assert_eq!(
            error.to_string(),
            format!(
                "{} is not available on Linux; use tmux or Herdr",
                kind.display_name()
            )
        );
        let error = INSTALLED.adapter(kind).err().unwrap();
        assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{kind:?}");
    }
}
#[test]
fn real_private_pty_owner_and_shell_are_attested() {
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    let root = tempfile::tempdir().unwrap();
    let result = root.path().join("result.json");
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let _master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut command = Command::new("/bin/bash");
    command.args(["--noprofile","--norc","-i","-c", "\"$AB_PTY_EXE\" --exact native::terminal::linux::tests::pty_owner_probe --test-threads=1; result=$?; exit \"$result\""])
        .env("AB_PTY_EXE",std::env::current_exe().unwrap()).env("AB_PTY_RESULT",&result)
        .stdin(Stdio::from(slave.try_clone().unwrap())).stdout(Stdio::from(slave.try_clone().unwrap())).stderr(Stdio::from(slave.try_clone().unwrap()));
    let fd = slave.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = command.status().unwrap();
    assert!(status.success(), "PTY probe failed: {status}");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
    assert!(value["linux_birth"].is_object());
    assert!(value["terminal_shell"]["linux_birth"].is_object());
}
#[test]
fn pty_owner_probe() {
    let Some(path) = std::env::var_os("AB_PTY_RESULT") else {
        return;
    };
    let owner = current_native_session_owner("session-pty").unwrap();
    let a = attest_owner(&owner);
    assert!(matches!(
        a.process,
        ProcessObservation::Live(Attested::Incarnation)
    ));
    assert!(matches!(a.control, ControlAttestation::Verified));
    let surface:TerminalSession=serde_json::from_value(serde_json::json!({"terminal":"iterm2","session_id":"test","managed_session_id":"session-pty"})).unwrap();
    verify_native_terminal_owner_with(
        "session-pty",
        &surface,
        owner.terminal_tty.as_deref(),
        owner.clone(),
        process::live_native_process_identity,
        process::terminal_tty_device,
    )
    .unwrap();
    assert!(
        verify_native_terminal_owner_with(
            "session-pty",
            &surface,
            Some("/dev/wrong"),
            owner.clone(),
            process::live_native_process_identity,
            process::terminal_tty_device
        )
        .is_err()
    );
    primitives::write_json_atomic(std::path::Path::new(&path), &owner).unwrap();
}

#[test]
fn linux_owner_stop_publishes_intent_rechecks_and_resumes_without_resignalling() {
    use super::super::{
        CloseOutcome,
        close_authority::{TerminalCloseAuthority, test_support::TestObservation},
    };
    use crate::native::session::close::{
        self,
        interruption::{self, Point},
    };
    use std::cell::Cell;
    for interrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-stop");
        std::fs::create_dir(&dir).unwrap();
        let store = Store::open_unchecked(&dir);
        update_status(&dir, SessionState::Ready, None, None).unwrap();
        let mut owner = owner();
        owner.managed_session_id = Some("session-stop".into());
        owner.terminal_tty = Some("/dev/pts/fixture".into());
        store.write_owner(&owner).unwrap();
        let surface:TerminalSession=serde_json::from_value(serde_json::json!({"terminal":"warp","session_id":"fixture","managed_session_id":"session-stop"})).unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        let proofs = Cell::new(0);
        let signals = Cell::new(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = || {
            stop_attested_owner(
                &store,
                "session-stop",
                &surface,
                OwnerStopIntent::Teardown,
                OwnerStop {
                    attest: || {
                        proofs.set(proofs.get() + 1);
                        Ok((owner.clone(), live()))
                    },
                    shell: ProvenShell,
                    record_incarnation: |_: &mut NativeSessionOwner| Ok(()),
                    stop: |groups: OwnerGroups<()>| {
                        assert_eq!(groups.owner, 42);
                        assert!(close::terminal_teardown_intent(&store, &surface)?.is_some());
                        signals.set(signals.get() + 1);
                        Ok(())
                    },
                },
                deadline,
            )
        };
        if interrupt {
            assert!(interruption::during(Point::IntentPublished, run).is_err());
            assert_eq!((proofs.get(), signals.get()), (1, 0));
            // Resume performs no stop. Both the exact intent and gone group are needed.
            let observation = TestObservation::on_records(&dir, "session-stop", &surface, deadline)
                .with_posix_owner_live(|_| OwnerAttestation {
                    process: ProcessObservation::Gone(Gone::Exited),
                    control: ControlAttestation::NotRecorded,
                    pid_alive: Some(false),
                })
                .with_pid_alive(|_| false)
                .with_group_gone(|group| {
                    assert_eq!(group, 42);
                    Ok(true)
                });
            assert!(matches!(
                observation
                    .decide("session-stop", &surface, deadline)
                    .unwrap(),
                TerminalCloseAuthority::TeardownComplete(_)
            ));
            assert_eq!(signals.get(), 0);
        } else {
            run().unwrap();
            assert_eq!((proofs.get(), signals.get()), (2, 1));
        }
        assert!(
            close::terminal_teardown_intent(&store, &surface)
                .unwrap()
                .is_some()
        );
        let result = super::super::close_authority::close_owned_surface_with(
            || Ok(TerminalCloseAuthority::Absent),
            || panic!("absent host has no owner action"),
            |_| panic!("absent host has no surface action"),
            deadline,
        )
        .unwrap();
        assert_eq!(result.outcome, CloseOutcome::Missing);
    }
}

#[test]
fn interrupted_created_host_close_consumes_records_without_a_second_signal() {
    use super::super::{
        CloseOutcome,
        close_authority::{self, test_support::TestObservation},
    };
    use std::cell::Cell;
    for intent in ["missing", "mismatched"] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-hostclose");
        std::fs::create_dir(&dir).unwrap();
        let store = Store::open_unchecked(&dir);
        update_status(&dir, SessionState::Ready, None, None).unwrap();
        let surface:TerminalSession=serde_json::from_value(serde_json::json!({"terminal":"warp","session_id":"fixture","managed_session_id":"session-hostclose"})).unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        if intent == "mismatched" {
            store
                .record(CoreRecord::TerminalCloseIntent)
                .write_json(&serde_json::json!({"wrong":"intent"}))
                .unwrap();
        }
        let host_live = Cell::new(true);
        let signals = Cell::new(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let run = || {
            close::close(&store, None, deadline, |handle| {
                let observation =
                    TestObservation::on_records(&dir, "session-hostclose", handle, deadline)
                        .with_created_host(|| {
                            Ok(Some(if host_live.get() {
                                ProcessObservation::Live(Attested::Incarnation)
                            } else {
                                ProcessObservation::Gone(Gone::Exited)
                            }))
                        });
                close_authority::close_owned_surface_with(
                    || observation.decide("session-hostclose", handle, deadline),
                    || panic!("no owner teardown"),
                    |close| {
                        assert_eq!(close, super::super::posix::OwnedSurfaceClose::SurfaceOnly);
                        signals.set(signals.get() + 1);
                        host_live.set(false);
                        panic!(
                            "deterministic interruption after host exit, before handle consumption"
                        )
                    },
                    deadline,
                )
            })
        };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(&run)).is_err());
        assert!(dir.join("terminal.closing.json").exists());
        assert_eq!(run().unwrap(), CloseOutcome::Missing);
        assert_eq!(signals.get(), 1);
        assert!(dir.join("terminal.closed.json").exists());
        assert!(!dir.join("terminal.closing.json").exists());
        assert!(
            !store
                .record(CoreRecord::TerminalCloseIntent)
                .path()
                .exists()
        );
        // Repeated close after consumption consults no host and cannot signal.
        assert_eq!(run().unwrap(), CloseOutcome::Missing);
        assert_eq!(signals.get(), 1);
    }
}

#[test]
fn created_host_observation_cannot_grant_close_after_the_deadline() {
    use super::super::close_authority::test_support::{RecordValues, TestObservation};
    use crate::native::{clock::FakeClock, session::close::budget};
    let now = Instant::now();
    let deadline = now + Duration::from_secs(1);
    let clock = FakeClock::new(now);
    let time = clock.shared();
    let _clock = budget::Scope::with_clock(clock);
    let surface: TerminalSession = serde_json::from_value(
        serde_json::json!({"session_id":"fixture","managed_session_id":"session-host"}),
    )
    .unwrap();
    let obs = TestObservation::from_values(RecordValues {
        owner: None,
        state: SessionState::Ready,
        launch: None,
    })
    .with_created_host(|| {
        time.set(deadline);
        Ok(Some(ProcessObservation::Live(Attested::Incarnation)))
    });
    assert!(obs.decide("session-host", &surface, deadline).is_err());
}

#[test]
fn shell_and_host_birth_wires_round_trip_without_reinterpreting_legacy_birth() {
    let mac = serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7,"process_start_seconds":100,"process_start_microseconds":0});
    let linux = serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7,"linux_birth":birth(10)});
    for value in [&mac, &linux] {
        let shell: TerminalShellIdentity = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&shell).unwrap(), *value);
    }
    for value in [
        serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7}),
        serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7,"process_start_seconds":100}),
        serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7,"linux_birth":null}),
        serde_json::json!({"pid":41,"process_group":41,"terminal_tty_device":7,"linux_birth":birth(10),"process_start_microseconds":0}),
    ] {
        assert!(serde_json::from_value::<TerminalShellIdentity>(value).is_err());
    }
    let handle =
        serde_json::json!({"session_id":"fixture","linux_host":{"pid":41,"linux_birth":birth(10)}});
    let surface: TerminalSession = serde_json::from_value(handle).unwrap();
    assert_eq!(surface.tmux_server().unwrap().linux_birth, birth(10));
    let mut reboot = birth(10);
    reboot.boot_id = "ffffffff-ffff-ffff-ffff-ffffffffffff".parse().unwrap();
    let a = attest_owner_with(
        &owner(),
        OwnerObservers {
            pid_alive: &|_| true,
            birth: &|_| Ok(Some(ProcessBirth::Linux(reboot))),
            control: &|_| panic!("reboot is not live control"),
        },
    );
    assert!(matches!(
        a.process,
        ProcessObservation::Gone(Gone::Reused(_))
    ));
}
