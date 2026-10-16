//! Terminal.app ancestry, incarnation and absence-proof tests. They walk this test's real
//! process ancestors and replay recorded AppleScript answers; moved here from
//! `native/tests/macos.rs` beside the adapter they exercise.
use crate::native::session::close as session_close;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::{SESSION_OWNER_FILE, TERMINAL_HANDLE_FILE};
use crate::native::session::{Reader, Store};
use crate::native::terminal;
use crate::native::terminal::macos::apple_terminal::{
    apple_terminal_startup_absent_with, record_legacy_terminal_app, terminal_app_ancestor,
    terminal_app_process, terminal_app_process_with, terminal_surface_absent,
};
use crate::native::terminal::macos::process::macos_process_start;
use crate::native::terminal::ownership::{
    MacTerminalAppIdentity, MacTerminalShellIdentity, NativeSessionOwner,
};
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
fn apple_terminal_app_capture_requires_stable_verified_shell_ancestry() {
    let shell = MacTerminalShellIdentity {
        pid: 4,
        process_group: 4,
        terminal_tty_device: 7,
        process_start_seconds: 100,
        process_start_microseconds: 42,
    };
    for mode in [
        "same",
        "reused-shell",
        "changed-parent",
        "unreadable",
        "foreign-app",
        "wrong-pid",
    ] {
        let mut visits = std::collections::HashMap::new();
        let result = terminal_app_process_with(&shell, |pid| {
            let count = visits.entry(pid).or_insert(0);
            *count += 1;
            if mode == "unreadable" && pid == 3 {
                bail!("injected unreadable ancestor");
            }
            let seconds = if mode == "reused-shell" && pid == 4 {
                101
            } else {
                100
            };
            let parent = if mode == "changed-parent" && *count > 1 && pid == 3 {
                1
            } else {
                pid - 1
            };
            let path = if pid == 2 && mode != "foreign-app" {
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"
            } else {
                "/bin/zsh"
            };
            Ok((
                MacTerminalAppIdentity {
                    pid: if mode == "wrong-pid" { 99 } else { pid },
                    start_seconds: seconds,
                    start_microseconds: 42,
                },
                parent,
                path.into(),
            ))
        });
        assert_eq!(result.is_ok(), mode == "same", "{mode}: {result:?}");
        if let Ok(app) = result {
            assert_eq!(app.pid, 2);
            assert_eq!(visits.get(&4), Some(&2));
        }
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

// The production capture on the real ancestors of this test. Under a terminal tab they
// include the root-owned /usr/bin/login, which PROC_PIDTBSDINFO does not answer for.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_reads_the_ancestors_of_any_user() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let owner = NativeSessionOwner {
        pid,
        terminal_shell: Some(MacTerminalShellIdentity {
            pid,
            process_group: 0,
            terminal_tty_device: 0,
            process_start_seconds: seconds,
            process_start_microseconds: microseconds,
        }),
        ..NativeSessionOwner::default()
    };
    match terminal_app_process(&owner) {
        // The suite itself runs in a Terminal.app tab.
        Ok(app) => assert_eq!(
            macos_process_start(app.pid).unwrap(),
            Some((app.start_seconds, app.start_microseconds))
        ),
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!("R2_LIFECYCLE terminal_app_process on this test's lineage: {error}");
            assert!(
                error.contains("has no system Terminal.app ancestor"),
                "an ancestor could not be read: {error}"
            );
        }
    }
}

// The capture's own walk and recheck, with the production observation, up to the
// ancestor that launchd started; that ancestor stands for Terminal.app, which no test
// may start. Under a terminal tab the walk crosses the root-owned /usr/bin/login.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_walks_this_tests_real_ancestors_to_the_app() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let shell = MacTerminalShellIdentity {
        pid,
        process_group: 0,
        terminal_tty_device: 0,
        process_start_seconds: seconds,
        process_start_microseconds: microseconds,
    };
    let mut lineage = Vec::new();
    let app = terminal_app_process_with(&shell, |pid| {
        let (identity, parent, path) = terminal_app_ancestor(pid)?;
        if !lineage.contains(&path) {
            lineage.push(path.clone());
        }
        let path = if parent == 1 {
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".to_owned()
        } else {
            path
        };
        Ok((identity, parent, path))
    })
    .unwrap();
    eprintln!("R2_LIFECYCLE real lineage of this test: {lineage:#?}");
    let (identity, parent, _) = terminal_app_ancestor(app.pid).unwrap();
    assert_eq!((identity, parent), (app, 1));
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

// The live owner of a record of 0.0.10 or earlier: the close derives the app incarnation
// from that owner's own ancestry and records it before the intent, which is then exact for
// the updated owner record. A derivation that fails changes nothing.
#[cfg(target_os = "macos")]
#[test]
fn legacy_terminal_owner_records_its_app_incarnation_before_the_close_intent() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = write_attested_apple_terminal_state(directory.path(), "ready", 4242);
    owner.terminal_app = None;
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    let stored_app = || {
        read_json::<NativeSessionOwner>(&directory.path().join(SESSION_OWNER_FILE))
            .unwrap()
            .terminal_app
    };

    let error = record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        bail!("the verified terminal shell has no system Terminal.app ancestor")
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no system Terminal.app ancestor")
    );
    assert!(owner.terminal_app.is_none() && stored_app().is_none());

    let app = terminal_instance(1234, 100);
    record_legacy_terminal_app(directory.path(), &mut owner, |legacy| {
        assert_eq!(legacy.pid, 4242);
        Ok(app.clone())
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app.clone()));
    session_close::record_terminal_close_intent(
        &Store::open_unchecked(directory.path()),
        "session-owner123",
        &handle,
        &owner,
    )
    .unwrap();
    assert_eq!(
        session_close::terminal_close_intent_owner(
            &Reader::open_unchecked(directory.path()),
            &handle,
            terminal::surface_outlives_owner(&handle)
        )
        .unwrap()
        .and_then(|owner| owner.terminal_app),
        Some(app.clone())
    );

    record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        panic!("a recorded incarnation is never derived again")
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app));
}

#[cfg(target_os = "macos")]
#[test]
fn attested_terminal_absence_does_not_accept_another_app_instances_reply() {
    let app = terminal_instance(1234, 100);
    let reads = std::cell::Cell::new(0);
    let result = terminal_surface_absent(
        Some(&app),
        |_| Ok(Some((app.start_seconds, app.start_microseconds))),
        || Ok(vec![app.clone(), terminal_instance(5678, 200)]),
        || {
            reads.set(reads.get() + 1);
            Ok(false)
        },
    );
    assert!(
        result.is_err(),
        "ambiguous instance established absence: {result:?}"
    );
    assert_eq!(
        reads.get(),
        0,
        "no app is queried when its address is ambiguous"
    );
}
