use super::*;
use crate::native::{
    clock::SystemClock,
    session::{CoreRecord, SessionState, update_status},
};
use std::time::Duration;
use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

fn bound_herdr() -> (tempfile::TempDir, Store, TerminalSession) {
    let (root, store, _) = launch::test_support::pending_launch(
        "session-host-",
        &std::env::temp_dir(),
        Duration::from_secs(30),
    );
    let id = store.directory().file_name().unwrap().to_str().unwrap();
    let mut surface = TerminalSession::new(TerminalKind::Herdr, "t7".into());
    surface.managed_session_id = Some(id.into());
    store
        .record(CoreRecord::Terminal)
        .write_json(&surface)
        .unwrap();
    (root, store, surface)
}

#[test]
fn a_bound_herdr_input_failure_settles_while_the_launch_is_still_pending() {
    let (_root, store, surface) = bound_herdr();
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
        let (_root, store, surface) = bound_herdr();
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
    for change in ["none", "closed", "claim", "binding", "environment"] {
        let (_root, store, surface) = bound_herdr();
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
