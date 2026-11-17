use super::*;
use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};
#[test]
fn admission_refuses_relative_link_shared_and_invalid_before_callback() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("session-private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.path().join("session-link");
    std::os::unix::fs::symlink(&private, &link).unwrap();
    let spec = BoundHostSpec { name: "test host" };
    for path in [Path::new("session-relative"), &link] {
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
    use crate::native::{
        clock::SystemClock,
        session::{CoreRecord, SessionState, launch, update_status},
    };
    use std::time::Duration;
    for change in ["none", "closed", "claim", "binding", "environment"] {
        let (_root, store, _) = launch::test_support::pending_launch(
            "session-host-",
            &std::env::temp_dir(),
            Duration::from_secs(30),
        );
        let id = store.directory().file_name().unwrap().to_str().unwrap();
        let surface: super::super::TerminalSession = serde_json::from_value(
            serde_json::json!({"terminal":"iterm2","session_id":"host","managed_session_id":id}),
        )
        .unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        let steps = std::cell::RefCell::new(Vec::new());
        let facts = launch::BindingFacts {
            host: "test",
            cancelled: "test cancelled",
            budget: launch::BindingBudget::RemainingReceiptCapped,
        };
        let result = prepare_bound_launch_with(
            store.directory(),
            &BoundHostSpec { name: "test" },
            &facts,
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
        );
        assert_eq!(result.is_ok(), change == "none", "{change}: {result:?}");
        assert_eq!(
            *steps.borrow(),
            if change == "none" {
                vec!["validate", "flush"]
            } else {
                vec!["validate"]
            }
        );
    }
}
