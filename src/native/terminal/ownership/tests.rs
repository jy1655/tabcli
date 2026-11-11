//! Owner-proof and process-group tests of the terminal ownership rules. They run on every
//! platform; moved here from `native/tests/close.rs` beside the rules they exercise.

use crate::native::terminal;
use crate::native::terminal::ownership::{
    ControlRecord, MacTerminalShellIdentity, NativeProcessIdentity, NativeSessionOwner,
    OwnerBinding, RecordedControl, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
    verify_native_terminal_owner_with,
};
use anyhow::{Context, bail};
use std::{cell::RefCell, path::Path};

const SESSION_ID: &str = "session-owner123";
const OWNER_TTY: &str = "/dev/ttys001";

fn owner_fixture() -> (
    terminal::TerminalSession,
    NativeSessionOwner,
    NativeProcessIdentity,
    NativeProcessIdentity,
) {
    let terminal = serde_json::from_value(serde_json::json!({
        "terminal": "apple-terminal",
        "session_id": OWNER_TTY,
        "window_id": "1001",
        "managed_session_id": SESSION_ID
    }))
    .unwrap();
    let live_shell = NativeProcessIdentity {
        pid: 4000,
        parent_pid: 3999,
        terminal_tty_device: 7,
        process_group: 4000,
        terminal_process_group: 4242,
        process_start_seconds: 90,
        process_start_microseconds: 100,
    };
    let owner = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION_ID.to_owned()),
        terminal_tty: Some(OWNER_TTY.to_owned()),
        terminal_tty_device: Some(7),
        process_start_seconds: Some(100),
        process_start_microseconds: Some(200),
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        terminal_shell: Some(MacTerminalShellIdentity {
            pid: live_shell.pid,
            process_group: live_shell.process_group,
            terminal_tty_device: live_shell.terminal_tty_device,
            process_start_seconds: live_shell.process_start_seconds,
            process_start_microseconds: live_shell.process_start_microseconds,
        }),
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
    (terminal, owner, live, live_shell)
}

#[test]
fn terminal_owner_verifier_observes_owner_tty_then_shell_for_multiple_terminals() {
    use terminal::TerminalKind;

    for (kind, binding) in [
        (TerminalKind::AppleTerminal, Some(SESSION_ID)),
        (TerminalKind::Iterm2, Some(SESSION_ID)),
        (TerminalKind::Iterm2, None),
        (TerminalKind::Ghostty, Some(SESSION_ID)),
        (TerminalKind::Warp, Some(SESSION_ID)),
        (TerminalKind::WezTerm, Some(SESSION_ID)),
    ] {
        for window_id in [None, Some("")] {
            for surface_tty in [None, Some(OWNER_TTY)] {
                for recorded_shell in [false, true] {
                    let (mut terminal, mut owner, live, shell) = owner_fixture();
                    terminal.kind = kind;
                    terminal.id = "surface-id-independent-of-tty".to_owned();
                    terminal.window_id = window_id.map(str::to_owned);
                    terminal.managed_session_id = binding.map(str::to_owned);
                    if !recorded_shell {
                        owner.terminal_shell = None;
                    }
                    let calls = RefCell::new(Vec::new());
                    let (verified_owner, verified_live) = verify_native_terminal_owner_with(
                        SESSION_ID,
                        &terminal,
                        surface_tty,
                        owner.clone(),
                        |pid| {
                            calls.borrow_mut().push(("process", pid));
                            match pid {
                                4242 => Ok(live),
                                4000 => Ok(shell),
                                _ => panic!("unexpected process {pid}"),
                            }
                        },
                        |path| {
                            assert_eq!(path, Path::new(OWNER_TTY));
                            calls.borrow_mut().push(("tty", 0));
                            Ok(7)
                        },
                    )
                    .unwrap();
                    assert_eq!(verified_live, live);
                    assert_eq!(
                        serde_json::to_value(verified_owner).unwrap(),
                        serde_json::to_value(owner).unwrap()
                    );
                    assert_eq!(
                        calls.into_inner(),
                        [("process", 4242), ("tty", 0), ("process", 4000)]
                    );
                }
            }
        }
    }
}

#[test]
fn terminal_owner_verifier_reports_live_owner_failure_before_binding() {
    let (mut terminal, mut owner, _, _) = owner_fixture();
    terminal.managed_session_id = None;
    owner.managed_session_id = None;
    owner.terminal_tty_device = None;
    let calls = RefCell::new(Vec::new());
    let error = verify_native_terminal_owner_with(
        SESSION_ID,
        &terminal,
        Some("/dev/ttys002"),
        owner,
        |pid| {
            calls.borrow_mut().push(pid);
            bail!("could not observe live owner")
        },
        |_| panic!("TTY observation after live-owner failure"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "could not observe live owner");
    assert_eq!(calls.into_inner(), [4242]);
}

#[test]
fn terminal_owner_verifier_checks_surface_then_owner_binding_before_identity() {
    for (surface_binding, owner_binding, expected_error) in [
        (
            Some("session-other456"),
            Some("session-other789"),
            "terminal handle belongs to managed session session-other456, not session-owner123",
        ),
        (
            None,
            None,
            "terminal handle is missing its managed session binding",
        ),
        (
            Some(SESSION_ID),
            Some("session-other456"),
            "native-session owner is not bound to this managed session",
        ),
        (
            Some(SESSION_ID),
            None,
            "native-session owner is not bound to this managed session",
        ),
    ] {
        let (mut terminal, mut owner, live, _) = owner_fixture();
        terminal.managed_session_id = surface_binding.map(str::to_owned);
        owner.managed_session_id = owner_binding.map(str::to_owned);
        owner.terminal_tty_device = None;
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            Some("/dev/ttys002"),
            owner,
            |pid| {
                calls.borrow_mut().push(pid);
                Ok(live)
            },
            |_| panic!("TTY observation after binding failure"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected_error);
        assert_eq!(calls.into_inner(), [4242]);
    }
}

#[test]
fn terminal_owner_verifier_requires_every_identity_field_before_tty_observation() {
    for present in 0..31 {
        let (terminal, mut owner, live, _) = owner_fixture();
        owner.terminal_tty_device = (present & 1 != 0).then_some(7);
        owner.process_start_seconds = (present & 2 != 0).then_some(100);
        owner.process_start_microseconds = (present & 4 != 0).then_some(200);
        owner.process_group = (present & 8 != 0).then_some(4242);
        owner.terminal_process_group = (present & 16 != 0).then_some(4242);
        assert_eq!(native_owner_identity_matches(&owner, &live), present == 0);
        owner.terminal_tty = None;
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            None,
            owner,
            |pid| {
                calls.borrow_mut().push(pid);
                Ok(live)
            },
            |_| panic!("TTY observation for incomplete identity {present:05b}"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "native-session owner birth, TTY, or foreground identity changed",
            "identity fields {present:05b}"
        );
        assert_eq!(calls.into_inner(), [4242]);
    }
}

#[test]
fn terminal_owner_verifier_rejects_reused_pid_and_changed_control_identity() {
    let (terminal, owner, live, _) = owner_fixture();
    assert!(native_owner_identity_matches(&owner, &live));
    for changed in [
        NativeProcessIdentity { pid: 4243, ..live },
        NativeProcessIdentity {
            process_start_seconds: 101,
            ..live
        },
        NativeProcessIdentity {
            process_start_microseconds: 201,
            ..live
        },
        NativeProcessIdentity {
            terminal_tty_device: 8,
            ..live
        },
        NativeProcessIdentity {
            process_group: 7777,
            ..live
        },
        NativeProcessIdentity {
            terminal_process_group: 7777,
            ..live
        },
    ] {
        assert!(!native_owner_identity_matches(&owner, &changed));
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            Some("/dev/ttys002"),
            owner.clone(),
            |pid| {
                calls.borrow_mut().push(pid);
                Ok(changed)
            },
            |_| panic!("TTY observation after identity mismatch"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "native-session owner birth, TTY, or foreground identity changed"
        );
        assert_eq!(calls.into_inner(), [4242]);
    }
}

#[test]
fn terminal_owner_verifier_requires_owner_tty_and_checks_optional_surface_tty_before_stat() {
    for (owner_tty, surface_tty, expected_error) in [
        (
            None,
            None,
            "native-session owner is missing its controlling TTY",
        ),
        (
            None,
            Some("/dev/ttys002"),
            "native-session owner is missing its controlling TTY",
        ),
        (
            Some(OWNER_TTY),
            Some("/dev/ttys002"),
            "terminal surface is attached to a different native-session TTY",
        ),
    ] {
        let (terminal, mut owner, live, _) = owner_fixture();
        owner.terminal_tty = owner_tty.map(str::to_owned);
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            surface_tty,
            owner,
            |pid| {
                calls.borrow_mut().push(pid);
                Ok(live)
            },
            |_| panic!("TTY stat after missing or mismatched TTY"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected_error);
        assert_eq!(calls.into_inner(), [4242]);
    }
}

#[test]
fn terminal_owner_verifier_preserves_tty_errors_before_group_and_shell_observation() {
    for (device, expected_error) in [
        (
            Ok(8),
            "native-session owner TTY path no longer identifies its controlling TTY",
        ),
        (
            Err(anyhow::anyhow!(
                "terminal TTY is not a character device: /dev/ttys001"
            )),
            "terminal TTY is not a character device: /dev/ttys001",
        ),
        (
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "stat denied",
            ))
            .context("failed to inspect terminal TTY /dev/ttys001"),
            "failed to inspect terminal TTY /dev/ttys001: stat denied",
        ),
    ] {
        let (terminal, mut owner, mut live, _) = owner_fixture();
        owner.process_group = Some(7777);
        owner.terminal_process_group = Some(7777);
        live.process_group = 7777;
        live.terminal_process_group = 7777;
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            None,
            owner,
            |pid| {
                calls.borrow_mut().push(("process", pid));
                assert_eq!(pid, 4242, "shell observation after TTY failure");
                Ok(live)
            },
            |path| {
                assert_eq!(path, Path::new(OWNER_TTY));
                calls.borrow_mut().push(("tty", 0));
                device
            },
        )
        .unwrap_err();
        assert_eq!(format!("{error:#}"), expected_error);
        if expected_error.ends_with("stat denied") {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        assert_eq!(calls.into_inner(), [("process", 4242), ("tty", 0)]);
    }
}

#[test]
fn terminal_owner_verifier_checks_owner_group_after_tty_before_shell() {
    let (terminal, mut owner, mut live, _) = owner_fixture();
    owner.process_group = Some(7777);
    owner.terminal_process_group = Some(7777);
    live.process_group = 7777;
    live.terminal_process_group = 7777;
    let calls = RefCell::new(Vec::new());
    let error = verify_native_terminal_owner_with(
        SESSION_ID,
        &terminal,
        None,
        owner,
        |pid| {
            calls.borrow_mut().push(("process", pid));
            assert_eq!(pid, 4242, "shell observation after owner-group failure");
            Ok(live)
        },
        |_| {
            calls.borrow_mut().push(("tty", 0));
            Ok(7)
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "native-session owner does not lead the terminal foreground process group"
    );
    assert_eq!(calls.into_inner(), [("process", 4242), ("tty", 0)]);
}

#[test]
fn terminal_owner_verifier_preserves_live_parent_shell_error_after_tty() {
    let (terminal, owner, live, _) = owner_fixture();
    let calls = RefCell::new(Vec::new());
    let error = verify_native_terminal_owner_with(
        SESSION_ID,
        &terminal,
        None,
        owner,
        |pid| {
            calls.borrow_mut().push(("process", pid));
            match pid {
                4242 => Ok(live),
                4000 => bail!("could not observe live parent shell"),
                _ => panic!("unexpected process {pid}"),
            }
        },
        |_| {
            calls.borrow_mut().push(("tty", 0));
            Ok(7)
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "could not observe live parent shell");
    assert_eq!(
        calls.into_inner(),
        [("process", 4242), ("tty", 0), ("process", 4000)]
    );
}

#[test]
fn terminal_owner_verifier_checks_shell_parent_tty_group_and_incarnation() {
    let (terminal, owner, live, shell) = owner_fixture();
    for (changed, expected_error) in [
        (
            NativeProcessIdentity { pid: 4001, ..shell },
            "Terminal.app shell no longer owns the native-session process",
        ),
        (
            NativeProcessIdentity {
                terminal_tty_device: 8,
                ..shell
            },
            "Terminal.app shell is attached to a different TTY",
        ),
        (
            NativeProcessIdentity {
                process_group: 4999,
                ..shell
            },
            "Terminal.app shell does not own the expected foreground job",
        ),
        (
            NativeProcessIdentity {
                terminal_process_group: 7777,
                ..shell
            },
            "Terminal.app shell does not own the expected foreground job",
        ),
        (
            NativeProcessIdentity {
                process_start_microseconds: 101,
                ..shell
            },
            "Terminal.app shell identity changed",
        ),
    ] {
        let calls = RefCell::new(Vec::new());
        let error = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            None,
            owner.clone(),
            |pid| {
                calls.borrow_mut().push(("process", pid));
                match pid {
                    4242 => Ok(live),
                    4000 => Ok(changed),
                    _ => panic!("unexpected process {pid}"),
                }
            },
            |_| {
                calls.borrow_mut().push(("tty", 0));
                Ok(7)
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected_error);
        assert_eq!(
            calls.into_inner(),
            [("process", 4242), ("tty", 0), ("process", 4000)]
        );
    }
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

/// What each platform's probe requires of a bound owner, stated over its attestation, on
/// every host. The parity tests below hold the macOS and Windows rows to the real probes.
#[test]
fn each_surface_probe_states_what_it_requires_of_a_bound_owner() {
    use crate::native::process_identity::{Attested, Gone, ProcessObservation};
    use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
    use crate::native::terminal::ownership::{OwnerRequirement, SurfaceProbe};
    let met = OwnerRequirement::Met;
    let control = OwnerRequirement::ControlUnverified;
    let gone = OwnerRequirement::Gone;
    let uninspected = OwnerRequirement::Uninspected;
    let live = || ProcessObservation::Live(Attested::Incarnation);
    // The answers of [MacosControl, WindowsProcess, Unprobed].
    let rows = [
        (live(), ControlAttestation::Verified, [met, met, met]),
        // Only macOS records a control identity that can change.
        (
            live(),
            ControlAttestation::Changed("process group changed".into()),
            [control, met, met],
        ),
        (
            live(),
            ControlAttestation::Unknown("tty unreadable".into()),
            [uninspected; 3],
        ),
        // A recorded birth without TTY and process groups.
        (
            live(),
            ControlAttestation::NotRecorded,
            [control, control, met],
        ),
        // A PID-only record; on Windows, a record without its process identity.
        (
            ProcessObservation::Live(Attested::PidOnly),
            ControlAttestation::NotRecorded,
            [control, control, met],
        ),
        (
            ProcessObservation::Gone(Gone::Exited),
            ControlAttestation::NotRecorded,
            [gone; 3],
        ),
        (
            ProcessObservation::Gone(Gone::Reused("another birth".into())),
            ControlAttestation::NotRecorded,
            [gone; 3],
        ),
        (
            ProcessObservation::Unknown("access denied".into()),
            ControlAttestation::NotRecorded,
            [uninspected; 3],
        ),
    ];
    for (process, control_attestation, expected) in rows {
        let attestation = OwnerAttestation {
            process,
            control: control_attestation,
            pid_alive: Some(true),
        };
        for (probe, expected) in [
            SurfaceProbe::MacosControl,
            SurfaceProbe::WindowsProcess,
            SurfaceProbe::Unprobed,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(
                probe.owner_requirement(&attestation),
                expected,
                "{probe:?} {attestation:?}"
            );
        }
    }
    assert_eq!(
        SurfaceProbe::HOST,
        if cfg!(target_os = "macos") {
            SurfaceProbe::MacosControl
        } else if cfg!(windows) {
            SurfaceProbe::WindowsProcess
        } else {
            SurfaceProbe::Unprobed
        }
    );
}

/// The macOS requirement agrees with the probe it states: for each owner record, the
/// record's attestation meets the requirement exactly when `verify_native_terminal_owner_with`
/// accepts the record. Both observe the same live processes, and the surface, TTY and shell
/// are the probe's accepting fixture, so the owner record and its live process are the only
/// differences. The attestation compares control identity on macOS only.
#[cfg(target_os = "macos")]
#[test]
fn the_macos_owner_requirement_agrees_with_the_probe() {
    use crate::native::session::owner::{OwnerObservers, attest_owner_with};
    use crate::native::terminal::ownership::{OwnerRequirement, SurfaceProbe};

    /// The recorded owner's process as both observers see it.
    #[derive(Clone, Copy, Debug)]
    enum Owner {
        Running(NativeProcessIdentity),
        Exited,
        /// Its birth could not be read.
        Uninspectable,
        /// Its birth was read, but not the snapshot carrying its control identity.
        ControlUnreadable(NativeProcessIdentity),
    }

    let (terminal, full, live, shell) = owner_fixture();
    let without_control = NativeSessionOwner {
        terminal_tty_device: None,
        process_group: None,
        terminal_process_group: None,
        ..full.clone()
    };
    let without_birth = NativeSessionOwner {
        process_start_seconds: None,
        process_start_microseconds: None,
        ..full.clone()
    };
    let pid_only = NativeSessionOwner {
        pid: full.pid,
        managed_session_id: full.managed_session_id.clone(),
        ..NativeSessionOwner::default()
    };
    let changed = |changed: NativeProcessIdentity| Owner::Running(changed);
    let cases = [
        (
            "full control identity",
            &full,
            Owner::Running(live),
            OwnerRequirement::Met,
        ),
        (
            "changed TTY",
            &full,
            changed(NativeProcessIdentity {
                terminal_tty_device: 8,
                ..live
            }),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "changed process group",
            &full,
            changed(NativeProcessIdentity {
                process_group: 7777,
                ..live
            }),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "changed terminal process group",
            &full,
            changed(NativeProcessIdentity {
                terminal_process_group: 7777,
                ..live
            }),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "birth without TTY and process groups",
            &without_control,
            Owner::Running(live),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "TTY and process groups without birth",
            &without_birth,
            Owner::Running(live),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "PID only",
            &pid_only,
            Owner::Running(live),
            OwnerRequirement::ControlUnverified,
        ),
        (
            "reused PID",
            &full,
            changed(NativeProcessIdentity {
                process_start_seconds: 101,
                ..live
            }),
            OwnerRequirement::Gone,
        ),
        ("exited", &full, Owner::Exited, OwnerRequirement::Gone),
        (
            "uninspectable birth",
            &full,
            Owner::Uninspectable,
            OwnerRequirement::Uninspected,
        ),
        (
            "unreadable control identity",
            &full,
            Owner::ControlUnreadable(live),
            OwnerRequirement::Uninspected,
        ),
    ];
    for (label, record, owner, expected) in cases {
        let snapshot = |pid: u32| -> anyhow::Result<Option<NativeProcessIdentity>> {
            assert_eq!(pid, record.pid, "{label}");
            match owner {
                Owner::Running(identity) => Ok(Some(identity)),
                Owner::Exited => Ok(None),
                Owner::Uninspectable | Owner::ControlUnreadable(_) => {
                    bail!("process {pid} could not be inspected")
                }
            }
        };
        let attestation = attest_owner_with(
            record,
            OwnerObservers {
                pid_alive: &|_| !matches!(owner, Owner::Exited),
                birth: &|pid| match owner {
                    Owner::Running(identity) | Owner::ControlUnreadable(identity) => Ok(Some((
                        identity.process_start_seconds,
                        identity.process_start_microseconds,
                    ))),
                    Owner::Exited | Owner::Uninspectable => snapshot(pid).map(|identity| {
                        identity.map(|i| (i.process_start_seconds, i.process_start_microseconds))
                    }),
                },
                control: &snapshot,
            },
        );
        let requirement = SurfaceProbe::MacosControl.owner_requirement(&attestation);
        assert_eq!(requirement, expected, "{label}: {attestation:?}");
        let probed = verify_native_terminal_owner_with(
            SESSION_ID,
            &terminal,
            Some(OWNER_TTY),
            record.clone(),
            |pid| {
                if pid == shell.pid {
                    return Ok(shell);
                }
                snapshot(pid)?.context("no such process")
            },
            |_| Ok(live.terminal_tty_device),
        );
        assert_eq!(
            requirement == OwnerRequirement::Met,
            probed.is_ok(),
            "{label}: {requirement:?} against the probe's {:?}",
            probed.map(|_| ())
        );
    }
}

/// The Windows requirement agrees with `verified_windows_native_owner` for this test
/// process's own owner record with, without and with a changed process identity.
#[cfg(windows)]
#[test]
fn the_windows_owner_requirement_agrees_with_the_probe() {
    use crate::native::session::owner::attest_owner;
    use crate::native::session::primitives::write_json_atomic;
    use crate::native::session::test_support::SESSION_OWNER_FILE;
    use crate::native::terminal::ownership::{
        OwnerRequirement, SurfaceProbe, current_native_session_owner, verified_windows_native_owner,
    };

    let directory = tempfile::tempdir().unwrap();
    let recorded = current_native_session_owner(SESSION_ID).unwrap();
    assert!(recorded.windows_process_identity.is_some());
    let without_identity = NativeSessionOwner {
        windows_process_identity: None,
        ..recorded.clone()
    };
    let mut changed_identity = recorded.clone();
    changed_identity
        .windows_process_identity
        .as_mut()
        .unwrap()
        .creation_time += 1;
    for (label, record, expected) in [
        ("recorded identity", recorded, OwnerRequirement::Met),
        (
            "no recorded identity",
            without_identity,
            OwnerRequirement::ControlUnverified,
        ),
        ("changed identity", changed_identity, OwnerRequirement::Gone),
    ] {
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &record).unwrap();
        let attestation = attest_owner(&record);
        let requirement = SurfaceProbe::WindowsProcess.owner_requirement(&attestation);
        assert_eq!(requirement, expected, "{label}: {attestation:?}");
        let probed = verified_windows_native_owner(directory.path(), SESSION_ID);
        assert_eq!(
            requirement == OwnerRequirement::Met,
            probed.is_ok(),
            "{label}: {requirement:?} against the probe's {:?}",
            probed.map(|_| ())
        );
    }
}

#[test]
fn owner_binding_answers_bound_unbound_or_foreign_for_the_expected_session() {
    for (recorded, expected) in [
        (Some(SESSION_ID), OwnerBinding::Bound),
        (None, OwnerBinding::Unbound),
        (Some("session-other456"), OwnerBinding::Foreign),
        (Some(""), OwnerBinding::Foreign),
    ] {
        let owner = NativeSessionOwner {
            managed_session_id: recorded.map(str::to_owned),
            ..NativeSessionOwner::default()
        };
        assert_eq!(owner.binding(SESSION_ID), expected, "{recorded:?}");
    }
}

// Every subset of the five control fields: none recorded, all five, or partial; and
// attestation's rule, which compares control when any of the TTY device and the two process
// groups is recorded. The two rules differ: a birth-only record is partial for the first and
// compares nothing for the second.
#[test]
fn control_record_and_attestation_rules_classify_every_subset_of_the_control_fields() {
    let (_, full, _, _) = owner_fixture();
    for present in 0..32u8 {
        let mut owner = full.clone();
        owner.terminal_tty_device = owner.terminal_tty_device.filter(|_| present & 1 != 0);
        owner.process_start_seconds = owner.process_start_seconds.filter(|_| present & 2 != 0);
        owner.process_start_microseconds = owner
            .process_start_microseconds
            .filter(|_| present & 4 != 0);
        owner.process_group = owner.process_group.filter(|_| present & 8 != 0);
        owner.terminal_process_group = owner.terminal_process_group.filter(|_| present & 16 != 0);
        let expected = match present {
            0 => ControlRecord::NotRecorded,
            31 => ControlRecord::Complete(RecordedControl {
                terminal_tty_device: 7,
                process_start_seconds: 100,
                process_start_microseconds: 200,
                process_group: 4242,
                terminal_process_group: 4242,
            }),
            _ => ControlRecord::Partial,
        };
        assert_eq!(owner.control_record(), expected, "{present:05b}");
        assert_eq!(
            owner.control_record().is_complete(),
            present == 31,
            "{present:05b}"
        );
        assert_eq!(
            owner.attestation_compares_control(),
            present & (1 | 8 | 16) != 0,
            "{present:05b}"
        );
    }
}
