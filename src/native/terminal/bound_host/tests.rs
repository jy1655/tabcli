use super::super::HerdrSurface;
use super::*;
use crate::native::{
    clock::SystemClock,
    session::{CoreRecord, SessionState, update_status},
};
use std::fs;
use std::time::Duration;
#[cfg(unix)]
use std::{cell::Cell, os::unix::fs::PermissionsExt};

const HOST_BIRTH: HerdrHostBirth = HerdrHostBirth::Macos {
    host_start_seconds: 1,
    host_start_microseconds: 0,
};

fn refusal_log(store: &Store) -> String {
    fs::read_to_string(store.directory().join(launch::LOG)).unwrap()
}

fn bound_fixture(kind: TerminalKind) -> (tempfile::TempDir, Store, TerminalSession) {
    let (root, store, _) = launch::test_support::pending_launch(
        "session-host-",
        &std::env::temp_dir(),
        Duration::from_secs(30),
    );
    let id = store.directory().file_name().unwrap().to_str().unwrap();
    let mut surface = TerminalSession::new(kind, "t7".into());
    if kind == TerminalKind::Herdr {
        surface = surface.with_herdr_core(
            HerdrSurface {
                socket: "test-endpoint".into(),
                core_id: "test-core".into(),
                created_ms: 1,
                host_pid: std::process::id(),
                birth: HOST_BIRTH,
            },
            "test-launch",
        );
    }
    surface.managed_session_id = Some(id.into());
    #[cfg(windows)]
    crate::native::session::primitives::set_private_directory_permissions(store.directory())
        .unwrap();
    store
        .record(CoreRecord::Terminal)
        .write_json(&surface)
        .unwrap();
    (root, store, surface)
}

#[test]
fn a_reused_herdr_host_pid_cannot_record_a_failure() {
    for birth in [
        Some(HerdrHostBirth::Macos {
            host_start_seconds: 2,
            host_start_microseconds: 0,
        }),
        None,
    ] {
        let (_root, store, surface) = bound_fixture(TerminalKind::Herdr);
        let error = run_handoff_with(
            store.directory(),
            TerminalKind::Herdr,
            &mut SystemClock,
            |bound| {
                assert_eq!(bound, &surface);
                assert_eq!(bound.herdr_core().unwrap().host_pid, std::process::id());
                Ok(())
            },
            || bail!("fixture could not flush startup input"),
            || -> Result<()> { panic!("failed input flush allowed handoff") },
            || birth,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "fixture could not flush startup input");
        assert!(
            refusal_log(&store)
                .contains("herdr_host_refused: fixture could not flush startup input")
        );
        assert!(
            store
                .record(CoreRecord::LaunchHost)
                .bytes()
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn direct_handoff_logs_a_directory_admission_refusal() {
    let root = tempfile::tempdir().unwrap();
    for kind in [TerminalKind::Herdr, TerminalKind::Tmux] {
        let directory = root.path().join(format!("session-{}", kind.as_str()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o770)).unwrap();
        let error = run_handoff_with(
            &directory,
            kind,
            &mut SystemClock,
            |_| panic!("admission refused"),
            || panic!("admission refused"),
            || -> Result<()> { panic!("admission refused") },
            || panic!("admission refused"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "{} launch directory is not private to the current user",
                kind.display_name()
            )
        );
        let log = fs::read_to_string(directory.join(launch::LOG)).unwrap();
        assert!(log.ends_with(&format!(" {}_host_refused: {error:#}\n", kind.as_str())));
        assert!(
            !Store::open_unchecked(&directory)
                .record(CoreRecord::LaunchHost)
                .path()
                .exists()
        );
    }
}

#[test]
fn a_bound_herdr_input_failure_settles_while_the_launch_is_still_pending() {
    let (_root, store, surface) = bound_fixture(TerminalKind::Herdr);
    let error = run_handoff_with(
        store.directory(),
        TerminalKind::Herdr,
        &mut SystemClock,
        |bound| {
            assert_eq!(bound, &surface);
            Ok(())
        },
        || bail!("fixture could not flush startup input"),
        || -> Result<()> { panic!("failed input flush allowed handoff") },
        || {
            assert!(
                refusal_log(&store)
                    .contains("herdr_host_refused: fixture could not flush startup input")
            );
            assert!(
                store
                    .record(CoreRecord::LaunchHost)
                    .bytes()
                    .unwrap()
                    .is_none()
            );
            Some(HOST_BIRTH)
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "fixture could not flush startup input");

    // No provider owner and no deadline expiry. A finished Herdr terminal remains present.
    assert!(launch::repair(&store).unwrap());
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert!(status.error.unwrap().contains("input_flush_failed"));
}

#[test]
fn a_late_herdr_handoff_failure_cannot_retire_a_changed_launch() {
    use launch::test_support::{Change, apply};
    for change in [
        "none",
        "close",
        "claim",
        "spawning",
        "binding",
        "already recorded",
    ] {
        let (_root, store, surface) = bound_fixture(TerminalKind::Herdr);
        let receipt = launch::read(&store).unwrap().unwrap();
        let fail = || {
            run_handoff_with(
                store.directory(),
                TerminalKind::Herdr,
                &mut SystemClock,
                |_| Ok(()),
                || Ok(()),
                || -> Result<()> {
                    match change {
                        "close" => apply(&store, &[Change::Tombstone]),
                        "claim" => apply(&store, &[Change::ClaimOther]),
                        "spawning" => apply(&store, &[Change::ReceiptSpawning]),
                        "binding" => {
                            let mut changed = surface.clone();
                            changed.id = "t8".into();
                            store.record(CoreRecord::Terminal).write_json(&changed)?;
                        }
                        _ => {}
                    }
                    bail!("fixture could not start shell")
                },
                || Some(HOST_BIRTH),
            )
            .unwrap_err()
        };
        let record = store.record(CoreRecord::LaunchHost);
        if change == "already recorded" {
            // Any existing stage or failure is authoritative for its own host.
            launch::host::initialize(&store, &receipt.claim_token, TerminalKind::Herdr).unwrap();
        }
        let before = record.bytes().unwrap();
        assert_eq!(
            fail().to_string(),
            "fixture could not start shell",
            "{change}"
        );
        if change == "none" {
            assert!(
                refusal_log(&store).contains("herdr_host_refused: fixture could not start shell")
            );
            assert!(matches!(launch::host::observe(&store, Some(&receipt)),
                launch::host::HostObservation::Recorded(host)
                if host.failure.as_ref().is_some_and(|failure| failure.reason == FailureReason::LaunchShellExecFailed)));
            let first = record.bytes().unwrap();
            assert_eq!(fail().to_string(), "fixture could not start shell");
            assert_eq!(record.bytes().unwrap(), first);
        } else {
            assert_eq!(record.bytes().unwrap(), before, "{change}");
        }
    }
}

#[cfg(unix)]
#[test]
fn admission_refuses_relative_link_shared_and_invalid_before_callback() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("session-private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.path().join("session-link");
    std::os::unix::fs::symlink(&private, &link).unwrap();
    let invalid = temp.path().join("invalid id");
    fs::create_dir(&invalid).unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o700)).unwrap();
    let spec = BoundHostSpec { name: "test host" };
    for path in [Path::new("session-relative"), &link, &invalid] {
        assert!(run_bound_host(path, &spec, |_| -> Result<()> { panic!("refused path") }).is_err());
    }
    let called = Cell::new(false);
    run_bound_host(&private, &spec, |id| {
        assert_eq!(id, "session-private");
        called.set(true);
        Ok(())
    })
    .unwrap();
    assert!(called.get());
    fs::set_permissions(&private, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(run_bound_host(&private, &spec, |_| -> Result<()> { panic!("shared path") }).is_err());
}

#[test]
fn handoff_rechecks_pending_claim_and_exact_binding_before_input_flush() {
    for change in [
        "none",
        "closed",
        "claim",
        "binding",
        "environment",
        "expired",
    ] {
        let (_root, store, surface) = bound_fixture(TerminalKind::Herdr);
        let steps = std::cell::RefCell::new(Vec::new());
        let result = run_handoff_with(
            store.directory(),
            TerminalKind::Herdr,
            &mut SystemClock,
            |bound| {
                assert_eq!(bound, &surface);
                steps.borrow_mut().push("validate");
                match change {
                    "closed" => update_status(store.directory(), SessionState::Closed, None, None)?,
                    "claim" => {
                        store.record(CoreRecord::TurnClaim).remove()?;
                        store
                            .record(CoreRecord::TurnClaim)
                            .write_private(b"different-claim")?;
                    }
                    "binding" => {
                        let mut changed = surface.clone();
                        changed.id = "replacement".into();
                        store.record(CoreRecord::Terminal).write_json(&changed)?;
                    }
                    "environment" => bail!("wrong host environment"),
                    "expired" => launch::test_support::apply(
                        &store,
                        &[launch::test_support::Change::ReceiptExpired],
                    ),
                    _ => {}
                }
                Ok(())
            },
            || {
                steps.borrow_mut().push("flush");
                Ok(())
            },
            || {
                steps.borrow_mut().push("handoff");
                Ok(())
            },
            || panic!("no recorded failure after validation"),
        );
        assert_eq!(result.is_ok(), change == "none", "{change}: {result:?}");
        assert!(
            store
                .record(CoreRecord::LaunchHost)
                .bytes()
                .unwrap()
                .is_none()
        );

        assert_eq!(
            *steps.borrow(),
            if change == "none" {
                vec!["validate", "flush", "handoff"]
            } else {
                vec!["validate"]
            }
        );
    }
}

#[cfg(unix)]
#[test]
fn a_tmux_handoff_failure_is_logged_without_a_host_record() {
    let (_root, store, _) = bound_fixture(TerminalKind::Tmux);
    let error = run_handoff_with(
        store.directory(),
        TerminalKind::Tmux,
        &mut SystemClock,
        |_| Ok(()),
        || Ok(()),
        || -> Result<()> { bail!("fixture could not start bash") },
        || panic!("tmux does not look up a Herdr process birth"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "fixture could not start bash");
    assert!(refusal_log(&store).contains("tmux_host_refused: fixture could not start bash"));
    assert!(
        store
            .record(CoreRecord::LaunchHost)
            .bytes()
            .unwrap()
            .is_none()
    );
}

// Every kind whose host runs the admission, and the Windows console, whose host does not.
#[cfg(unix)]
const KINDS: [TerminalKind; 8] = [
    TerminalKind::Herdr,
    TerminalKind::Tmux,
    TerminalKind::Iterm2,
    TerminalKind::WezTerm,
    TerminalKind::AppleTerminal,
    TerminalKind::Ghostty,
    TerminalKind::Warp,
    TerminalKind::WindowsConsole,
];

// The launcher's check is the host's own admission, for the same account: it refuses exactly
// the directories that admission refuses, with the host's reason, and never a directory of a
// kind whose host does not run it.
#[cfg(unix)]
#[test]
fn the_launcher_refuses_exactly_what_the_launch_host_would_refuse() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("session-private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let shared = temp.path().join("session-shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o750)).unwrap();
    let link = temp.path().join("session-link");
    std::os::unix::fs::symlink(&private, &link).unwrap();
    let invalid = temp.path().join("invalid id");
    fs::create_dir(&invalid).unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o700)).unwrap();
    let missing = temp.path().join("session-missing");
    for directory in [
        private.as_path(),
        &shared,
        &link,
        &invalid,
        &missing,
        Path::new("session-relative"),
    ] {
        for kind in KINDS {
            let spec = BoundHostSpec {
                name: kind.display_name(),
            };
            let host = run_bound_host(directory, &spec, |_| Ok(()));
            let launcher = refuse_before_launch(directory, kind);
            if kind == TerminalKind::WindowsConsole {
                launcher.unwrap();
                continue;
            }
            match host {
                Ok(()) => launcher.unwrap(),
                Err(refusal) => assert_eq!(
                    format!("{:#}", launcher.unwrap_err()),
                    format!(
                        "the {} launch host would refuse the session directory {}: {refusal:#}",
                        kind.display_name(),
                        directory.display()
                    ),
                    "{kind:?} {}",
                    directory.display()
                ),
            }
        }
    }
    assert!(refuse_before_launch(&private, TerminalKind::Herdr).is_ok());
    assert_eq!(
        format!(
            "{:#}",
            refuse_before_launch(&shared, TerminalKind::Herdr).unwrap_err()
        ),
        format!(
            "the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user",
            shared.display()
        )
    );
}

// A host that a core starts runs as the core: the check is made for the core's account, and
// when this process's own account would be admitted the refusal names both accounts.
#[cfg(unix)]
#[test]
fn a_core_started_host_is_judged_for_the_cores_account() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("session-core");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let own = unsafe { libc::geteuid() };
    let this = HostAccount::of_uid(own);
    refuse_before_core_launch(&private, TerminalKind::Herdr, &this).unwrap();
    let other = HostAccount::of_uid(own.wrapping_add(1));
    assert_eq!(
        format!(
            "{:#}",
            refuse_before_core_launch(&private, TerminalKind::Herdr, &other).unwrap_err()
        ),
        format!(
            "the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user; TAB runs as uid {own} and the Herdr Core as uid {}; run TAB as the core's user",
            private.display(),
            own.wrapping_add(1)
        )
    );
    // A directory neither account would admit is refused for its own reason alone.
    fs::set_permissions(&private, fs::Permissions::from_mode(0o770)).unwrap();
    let text = format!(
        "{:#}",
        refuse_before_core_launch(&private, TerminalKind::Herdr, &other).unwrap_err()
    );
    assert!(text.ends_with("not private to the current user"), "{text}");
}

// What the refusal tells a user whose TAB and core give their files different owners on
// Windows: the observed case (TAB elevated, the core not) names elevation and what to do.
#[test]
fn the_elevation_advice_names_the_observed_case() {
    let observed = elevation_advice("Herdr", Some(true), Some(false));
    assert!(
        observed.starts_with("TAB is running elevated and the Herdr Core is not:"),
        "{observed}"
    );
    assert!(
        observed.contains("Run TAB without elevation, as the core runs"),
        "{observed}"
    );
    for (tab, core, words) in [
        (
            Some(false),
            Some(true),
            "TAB (not elevated) and the Herdr Core (elevated)",
        ),
        (
            Some(true),
            Some(true),
            "TAB (elevated) and the Herdr Core (elevated)",
        ),
        (
            None,
            Some(false),
            "TAB (elevation unknown) and the Herdr Core (not elevated)",
        ),
    ] {
        let advice = elevation_advice("Herdr", tab, core);
        assert!(advice.starts_with(words), "{advice}");
        assert!(
            advice.ends_with("run TAB with the core's account and elevation"),
            "{advice}"
        );
    }
}
