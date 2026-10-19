//! Owner-proof and process-group tests of the terminal ownership rules. They run on every
//! platform; moved here from `native/tests/close.rs` beside the rules they exercise.

use crate::native::terminal;
use crate::native::terminal::ownership::{
    MacTerminalShellIdentity, NativeProcessIdentity, NativeSessionOwner,
    native_owner_identity_matches, verified_terminal_owner_process_group,
    verified_terminal_shell_process_group, verify_native_terminal_owner_with,
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
