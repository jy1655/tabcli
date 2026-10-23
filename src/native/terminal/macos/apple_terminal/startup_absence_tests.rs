//! Terminal.app failed-start absence tests. They replay recorded observations against the
//! adapter's startup records; moved here from `apple_terminal/ancestry_tests.rs`, whose
//! ancestry and incarnation tests now live beside `incarnation.rs`.
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::{SESSION_OWNER_FILE, TERMINAL_HANDLE_FILE};
use crate::native::terminal;
use crate::native::terminal::macos::apple_terminal::apple_terminal_startup_absent_with;
use crate::native::terminal::ownership::MacTerminalAppIdentity;
use crate::native::tests::{reaped_child_pid, write_attested_apple_terminal_state};
use anyhow::bail;
use std::fs;

#[cfg(target_os = "macos")]
fn terminal_instance(pid: u32, start_seconds: u64) -> MacTerminalAppIdentity {
    MacTerminalAppIdentity {
        pid,
        start_seconds,
        start_microseconds: 42,
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_uses_only_proven_read_only_absence() {
    for mode in [
        "no-owner",
        "no-app",
        "live-owner",
        "app-dead",
        "window-absent",
        "present",
        "reused",
        "unreadable",
        "wrong-window",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
        owner.terminal_app = (mode != "no-app").then_some(MacTerminalAppIdentity {
            pid: 1234,
            start_seconds: 100,
            start_microseconds: 42,
        });
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        if mode == "no-owner" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
        if mode == "wrong-window" {
            handle.window_id = Some("9999".into());
        }
        let observations = std::cell::Cell::new(0);
        let result = apple_terminal_startup_absent_with(
            directory.path(),
            &handle,
            |_| mode == "live-owner",
            |_| match mode {
                "app-dead" => Ok(None),
                "reused" => Ok(Some((101, 42))),
                "unreadable" => bail!("injected OS error"),
                _ => Ok(Some((100, 42))),
            },
            || {
                let mut instances = vec![terminal_instance(1234, 100)];
                if matches!(mode, "no-owner" | "no-app") {
                    instances.push(terminal_instance(5678, 200));
                }
                Ok(instances)
            },
            || {
                observations.set(observations.get() + 1);
                Ok(mode != "window-absent")
            },
        );
        assert_eq!(
            matches!(result, Ok(true)),
            matches!(mode, "app-dead" | "window-absent"),
            "{mode}: {result:?}"
        );
        assert_eq!(
            observations.get(),
            usize::from(matches!(mode, "window-absent" | "present")),
            "{mode}"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    }
}

// The rollback of a failed start asks the same question before the wrapper has recorded
// an owner or an app incarnation. It reads only, and it never touches the binding.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_without_app_incarnation_needs_one_stable_instance() {
    for shape in ["no-owner", "no-app"] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "two-instances",
            "restarted",
            "listing-error",
            "presence-error",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
            owner.terminal_app = None;
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if shape == "no-owner" {
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            }
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
            let listings = std::cell::Cell::new(0);
            let result = apple_terminal_startup_absent_with(
                directory.path(),
                &handle,
                |_| false,
                |_| panic!("{shape}/{mode}: no app incarnation is recorded"),
                || {
                    listings.set(listings.get() + 1);
                    match mode {
                        "listing-error" => bail!("injected process table error"),
                        "none-running" => Ok(Vec::new()),
                        "two-instances" => Ok(vec![
                            terminal_instance(1234, 100),
                            terminal_instance(5678, 200),
                        ]),
                        "restarted" if listings.get() > 1 => Ok(vec![terminal_instance(1234, 101)]),
                        _ => Ok(vec![terminal_instance(1234, 100)]),
                    }
                },
                || match mode {
                    "presence-error" => bail!("injected unreadable window list"),
                    "one-stable-listed" => Ok(true),
                    _ => Ok(false),
                },
            );
            match mode {
                "none-running" | "one-stable" => {
                    assert!(matches!(result, Ok(true)), "{shape}/{mode}")
                }
                "one-stable-listed" => assert!(matches!(result, Ok(false)), "{shape}/{mode}"),
                _ => assert!(result.is_err(), "{shape}/{mode}: {result:?}"),
            }
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}
