//! Terminal.app ancestry and incarnation tests. They walk this test's real process
//! ancestors and replay recorded observations; moved here from `apple_terminal/ancestry_tests.rs`
//! with the incarnation they exercise.
use super::{
    around_close, record_legacy_terminal_app, record_terminal_app_for_close_with,
    terminal_app_alive_with, terminal_app_ancestor, terminal_app_process,
    terminal_app_process_with, terminal_surface_absent,
};
use crate::native::session::close as session_close;
use crate::native::session::primitives::{read_json, write_json_atomic};
use crate::native::session::test_support::{SESSION_OWNER_FILE, TERMINAL_HANDLE_FILE};
use crate::native::session::{Reader, Store};
use crate::native::terminal;
use crate::native::terminal::macos::process::macos_process_start;
use crate::native::terminal::ownership::{
    MacTerminalAppIdentity, MacTerminalShellIdentity, NativeSessionOwner,
};
use crate::native::tests::write_attested_apple_terminal_state;
use anyhow::bail;

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

#[test]
fn terminal_app_inspection_error_preserves_reason() {
    let app = MacTerminalAppIdentity {
        pid: 42,
        start_seconds: 100,
        start_microseconds: 123,
    };
    let error = terminal_app_alive_with(&app, |_| {
        Err(anyhow::anyhow!("birth unreadable").context("inspection"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "inspection: birth unreadable");
}

// The order of one close transaction, now owned here: deadline, birth, instances, deadline,
// effect, deadline, birth, instances, deadline. A deadline reached during a step stops
// the bracket at the next deadline check, and an ended incarnation stops it at once,
// without reading the instances.
#[cfg(target_os = "macos")]
#[test]
fn close_bracket_orders_deadline_and_incarnation_checks_around_the_effect() {
    use crate::native::clock::FakeClock;
    use crate::native::session::close::budget::Scope;
    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    const FULL: [&str; 5] = ["birth", "instances", "effect", "birth", "instances"];
    let app = terminal_instance(1234, 100);
    // (deadline reached after this many steps, birth that finds the app ended, steps
    // taken, outcome)
    let cases = [
        (None, None, 5, "reply"),
        (Some(0), None, 0, "deadline"),
        (Some(1), None, 2, "deadline"),
        (Some(2), None, 2, "deadline"),
        (Some(3), None, 3, "deadline"),
        (Some(4), None, 5, "deadline"),
        (Some(5), None, 5, "deadline"),
        (None, Some(1), 1, "ended"),
        (None, Some(2), 4, "ended"),
    ];
    for (expire_after, ended_at, steps, outcome) in cases {
        let case = format!("{expire_after:?}/{ended_at:?}");
        let now = Instant::now();
        let fake = FakeClock::new(now);
        let clock = fake.shared();
        let _scope = Scope::with_clock(fake);
        let deadline = now + Duration::from_secs(1);
        let trace = RefCell::new(Vec::new());
        let step = |name: &'static str| {
            trace.borrow_mut().push(name);
            if expire_after == Some(trace.borrow().len()) {
                clock.set(deadline);
            }
        };
        if expire_after == Some(0) {
            clock.set(deadline);
        }
        let births = Cell::new(0);
        let result = around_close(
            &app,
            deadline,
            |pid| {
                assert_eq!(pid, app.pid);
                step("birth");
                births.set(births.get() + 1);
                Ok((ended_at != Some(births.get()))
                    .then_some((app.start_seconds, app.start_microseconds)))
            },
            || {
                step("instances");
                Ok(vec![app.clone()])
            },
            || {
                step("effect");
                Ok("reply".to_owned())
            },
        );
        assert_eq!(*trace.borrow(), &FULL[..steps], "{case}");
        match (outcome, result) {
            ("reply", Ok(Some(reply))) => assert_eq!(reply, "reply", "{case}"),
            ("ended", Ok(None)) => {}
            ("deadline", Err(error)) => assert!(
                format!("{error:#}").contains("close deadline expired"),
                "{case}: {error:#}"
            ),
            (_, result) => panic!("{case}: {result:?}"),
        }
    }
}

// The close of a live owner persists a legacy owner's incarnation before it asks whether
// that app is the only running Terminal; a recorded incarnation is not derived again, and
// a derivation that fails reads no instances and records nothing.
#[cfg(target_os = "macos")]
#[test]
fn owner_close_records_a_legacy_incarnation_before_requiring_a_unique_app() {
    use std::cell::RefCell;

    let app = terminal_instance(1234, 100);
    for mode in [
        "legacy",
        "legacy-ambiguous",
        "legacy-underivable",
        "recorded",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner = write_attested_apple_terminal_state(directory.path(), "ready", 4242);
        owner.terminal_app = (mode == "recorded").then_some(app.clone());
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let stored_app = || {
            read_json::<NativeSessionOwner>(&directory.path().join(SESSION_OWNER_FILE))
                .unwrap()
                .terminal_app
        };
        let trace = RefCell::new(Vec::new());
        let result = record_terminal_app_for_close_with(
            directory.path(),
            &mut owner,
            |legacy| {
                trace.borrow_mut().push("derive");
                assert_eq!(legacy.pid, 4242, "{mode}");
                assert!(
                    mode != "recorded",
                    "a recorded incarnation is never derived again"
                );
                if mode == "legacy-underivable" {
                    bail!("the verified terminal shell has no system Terminal.app ancestor")
                }
                Ok(app.clone())
            },
            || {
                trace.borrow_mut().push("instances");
                assert_eq!(stored_app(), Some(app.clone()), "{mode}: recorded first");
                let mut instances = vec![app.clone()];
                if mode == "legacy-ambiguous" {
                    instances.push(terminal_instance(5678, 200));
                }
                Ok(instances)
            },
        );
        let (expected, error, stored) = match mode {
            "legacy" => (&["derive", "instances"][..], None, Some(app.clone())),
            "legacy-ambiguous" => (
                &["derive", "instances"][..],
                Some("scripting target is ambiguous or changed"),
                Some(app.clone()),
            ),
            "legacy-underivable" => (
                &["derive"][..],
                Some("no system Terminal.app ancestor"),
                None,
            ),
            _ => (&["instances"][..], None, Some(app.clone())),
        };
        assert_eq!(*trace.borrow(), expected, "{mode}");
        match (result, error) {
            (Ok(()), None) => {}
            (Err(actual), Some(error)) => {
                assert!(actual.to_string().contains(error), "{mode}: {actual:#}")
            }
            (result, _) => panic!("{mode}: {result:?}"),
        }
        assert_eq!(owner.terminal_app, stored, "{mode}");
        assert_eq!(stored_app(), stored, "{mode}");
    }
}
