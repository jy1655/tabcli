//! #399: the Herdr terminal ended outside TAB (its tab or pane was closed, `hc kill`, or its
//! program exited) and took the native-session owner with it. An explicit close runs the
//! session's real close records and decision over the scripted core; only the OS observation
//! of the dead owner is injected.
use super::*;
use crate::native::process_identity::{Gone, ProcessObservation};
use crate::native::session::Store;
use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
use crate::native::session::{self, CoreRecord, SessionState};
use crate::native::terminal::close_authority::{CloseObservation, Production};
use crate::native::terminal::ownership::NativeSessionOwner;
use crate::native::terminal::posix::{self, PosixTerminalAdapter};

fn gone(gone: Gone) -> OwnerAttestation {
    OwnerAttestation {
        process: ProcessObservation::Gone(gone),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(false),
    }
}

/// The owner as the wrapper inside the terminal recorded it: bound, with its whole control
/// identity, so that only the terminal decides the dead-owner close.
fn dead_owner() -> NativeSessionOwner {
    NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        terminal_tty: Some("/dev/ttys042".into()),
        terminal_tty_device: Some(42),
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        #[cfg(target_os = "macos")]
        process_start_seconds: Some(1_790_000_000),
        #[cfg(target_os = "macos")]
        process_start_microseconds: Some(42),
        #[cfg(target_os = "linux")]
        linux_birth: Some(crate::native::terminal::LinuxBirth {
            boot_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
            starttime_ticks: 42,
        }),
        ..NativeSessionOwner::default()
    }
}

impl posix::Adapters for Adapter<FakeCore> {
    fn adapter(&self, kind: TerminalKind) -> Result<&dyn PosixTerminalAdapter> {
        assert_eq!(kind, TerminalKind::Herdr);
        Ok(self)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    directory: PathBuf,
    core: FakeCore,
}

#[test]
#[should_panic(expected = "scripted Herdr dead-owner Close cannot verify live ownership")]
fn scripted_dead_owner_close_refuses_the_installed_ownership_probe() {
    let fixture = Fixture::new();
    // A foreign binding makes the old ownership probe return before contacting any core.
    let surface = fixture
        .store()
        .terminal()
        .unwrap()
        .with_binding(Some("session-foreign".into()));
    let adapters = Adapter::with_core(fixture.core.clone());
    let observation = Production::new(&fixture.directory, SESSION, &surface, later())
        .with_posix_dead_owner_observations(&adapters, &|_| gone(Gone::Exited), &|_| false);
    let _ = observation.ownership_proof();
}

impl Fixture {
    /// A ready session whose Herdr terminal `t7` TAB created on the scripted core.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(SESSION);
        fs::create_dir(&directory).unwrap();
        let core = FakeCore::new();
        let surface = opened(&core, &directory);
        let store = Store::open_unchecked(&directory);
        session::update_status(&directory, SessionState::Ready, None, None).unwrap();
        store
            .record(CoreRecord::Owner)
            .write_json(&dead_owner())
            .unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        core.clear_calls();
        Self {
            _root: root,
            directory,
            core,
        }
    }

    fn store(&self) -> Store {
        Store::open_unchecked(&self.directory)
    }

    /// The terminal ends outside TAB: the core keeps it, finished, with its program PID.
    fn finish(&self) {
        self.core.set("t7", "state", json!("finished"));
    }

    /// `tabcli close-session --explicit` with the dead owner observed as `owner`.
    fn close(&self, owner: fn() -> OwnerAttestation, pid_alive: bool) -> Result<CloseOutcome> {
        let deadline = session::close::budget::close_deadline();
        let observe = |_: &NativeSessionOwner| owner();
        let pid_is_alive = |_| pid_alive;
        let adapters = Adapter::with_core(self.core.clone());
        session::close::close_with_attestation(&self.store(), None, deadline, observe, |surface| {
            Production::new(&self.directory, SESSION, surface, deadline)
                .with_posix_dead_owner_observations(&adapters, &observe, &pid_is_alive)
                .close_posix_surface()
        })
    }

    fn close_dead(&self) -> Result<CloseOutcome> {
        self.close(|| gone(Gone::Exited), false)
    }

    fn mutations(&self) -> (usize, usize) {
        (
            self.core.params("terminal.kill").len(),
            self.core.params("terminal.rm").len(),
        )
    }

    /// The close changed nothing: the handle is back, the session is ready.
    fn assert_retained(&self, label: &str) {
        let store = self.store();
        assert!(
            store.record(CoreRecord::Terminal).path().exists(),
            "{label}: the handle must be retained"
        );
        assert_eq!(
            store.status().unwrap().state,
            SessionState::Ready,
            "{label}"
        );
    }

    fn assert_closed(&self, label: &str) {
        let store = self.store();
        assert!(
            !store.record(CoreRecord::Terminal).path().exists(),
            "{label}: the handle must be consumed"
        );
        assert!(
            store.record(CoreRecord::TerminalClosed).path().exists(),
            "{label}: the close must leave its tombstone"
        );
        assert_eq!(
            store.status().unwrap().state,
            SessionState::Closed,
            "{label}"
        );
    }
}

#[test]
fn a_terminal_finished_outside_bridge_is_removed_by_an_explicit_close() {
    let fixture = Fixture::new();
    fixture.finish();
    // The generic probes stay read-only: presence finds the finished terminal, the live
    // operations refuse it, and none of them ends or removes anything.
    assert!(
        Adapter::with_core(fixture.core.clone())
            .present(
                &fixture.directory,
                &fixture.store().terminal().unwrap(),
                Duration::from_secs(1),
            )
            .unwrap()
    );
    let handle = fixture.store().terminal().unwrap();
    let text = error_text(
        &Adapter::with_core(fixture.core.clone())
            .verify(&handle, None)
            .unwrap_err(),
    );
    assert!(text.contains("has finished"), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
    assert!(fixture.core.terminal("t7").is_some());

    fixture.core.clear_calls();
    let outcome = fixture.close_dead();
    assert!(
        matches!(outcome, Ok(CloseOutcome::Closed)),
        "{:?}",
        outcome.map_err(|error| error_text(&error))
    );
    // The decision reads the terminal; the close reads it again, removes it once without a
    // kill, and confirms that the core no longer holds it.
    assert_eq!(
        fixture.core.methods(),
        [
            "connect",
            "hello",
            "terminal.get",
            "connect",
            "hello",
            "terminal.get",
            "terminal.rm",
            "terminal.get",
        ]
    );
    assert_eq!(
        fixture.core.params("terminal.rm"),
        [json!({"terminal": "t7"})]
    );
    assert!(fixture.core.terminal("t7").is_none());
    fixture.assert_closed("first close");
    // A repeated close finds the session closed and asks the core nothing.
    fixture.core.clear_calls();
    assert!(fixture.close_dead().is_ok());
    assert!(
        fixture.core.methods().is_empty(),
        "{:?}",
        fixture.core.methods()
    );
    fixture.assert_closed("repeated close");
}

// Rows that hold before and after the fix: a dead owner never ends a running terminal, and
// nothing but the finished terminal this session created is removed. Each refusal keeps the
// handle and the session, and sends nothing to the core.
#[test]
fn a_dead_owner_never_ends_or_removes_anything_but_its_finished_terminal() {
    type Change = fn(&Fixture);
    type Owner = fn() -> OwnerAttestation;
    let rows: [(&str, Change, Owner, bool, &str); 10] = [
        (
            "running terminal",
            |_| {},
            || gone(Gone::Exited),
            false,
            "no close was sent",
        ),
        (
            "kept",
            |f| {
                f.finish();
                f.core.set("t7", "keep", json!(true));
            },
            || gone(Gone::Exited),
            false,
            "set to keep",
        ),
        (
            "replacement program",
            |f| {
                f.finish();
                f.core.set("t7", "pid", json!(4242));
            },
            || gone(Gone::Exited),
            false,
            "not the terminal this session created",
        ),
        (
            "another creation",
            |f| {
                f.finish();
                f.core.set("t7", "created_ms", json!(1));
            },
            || gone(Gone::Exited),
            false,
            "not the terminal this session created",
        ),
        (
            "another session's directory",
            |f| {
                f.finish();
                let mut argv = f.core.terminal("t7").unwrap()["argv"].clone();
                argv[2] = json!("/state/session-other");
                f.core.set("t7", "argv", argv);
            },
            || gone(Gone::Exited),
            false,
            "not the terminal this session created",
        ),
        (
            "another core",
            |f| {
                f.finish();
                f.core.0.borrow_mut().core_id = "core-b".into();
            },
            || gone(Gone::Exited),
            false,
            "a different Herdr Core (id core-b)",
        ),
        (
            "no core",
            |f| {
                f.finish();
                f.core.0.borrow_mut().running = false;
            },
            || gone(Gone::Exited),
            false,
            "no Herdr Core is running",
        ),
        (
            "owner PID reused",
            Fixture::finish,
            || gone(Gone::Reused("different birth".into())),
            false,
            "its PID is reused",
        ),
        (
            "owner PID still alive",
            Fixture::finish,
            || gone(Gone::Exited),
            true,
            "its PID is reused",
        ),
        (
            "owner not inspectable",
            Fixture::finish,
            || OwnerAttestation {
                process: ProcessObservation::Unknown("owner inspection failed".into()),
                control: ControlAttestation::NotRecorded,
                pid_alive: None,
            },
            false,
            "owner inspection failed",
        ),
    ];
    for (label, change, owner, pid_alive, expected) in rows {
        let fixture = Fixture::new();
        change(&fixture);
        let before = fixture.core.terminal("t7");
        fixture.core.clear_calls();
        let text = error_text(&fixture.close(owner, pid_alive).unwrap_err());
        assert!(text.contains(expected), "{label}: {text}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        assert_eq!(fixture.core.terminal("t7"), before, "{label}");
        fixture.assert_retained(label);
    }
}

// Every read before the removal is checked again: a terminal that another client of the same
// user changes between the decision and the removal is left alone, and one that is already
// gone is absent.
#[test]
fn an_ended_terminal_that_changes_before_its_removal_is_left_alone() {
    type Change = fn(&mut BTreeMap<String, Value>);
    let rows: [(&str, Change, Option<&str>); 4] = [
        (
            "kept",
            |terminals| terminals.get_mut("t7").unwrap()["keep"] = json!(true),
            Some("set to keep"),
        ),
        (
            "replacement program",
            |terminals| {
                let terminal = terminals.get_mut("t7").unwrap();
                terminal["state"] = json!("live");
                terminal["pid"] = json!(4242);
            },
            Some("not the terminal this session created"),
        ),
        (
            "running again",
            |terminals| terminals.get_mut("t7").unwrap()["state"] = json!("live"),
            Some("is still running although its native-session owner is no longer live"),
        ),
        (
            "removed by someone else",
            |terminals| {
                terminals.remove("t7");
            },
            None,
        ),
    ];
    for (label, change, expected) in rows {
        let fixture = Fixture::new();
        fixture.finish();
        // The decision reads the terminal once; the change lands before the next read.
        fixture.core.after_call("terminal.get", change);
        let result = fixture.close_dead();
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        match expected {
            Some(expected) => {
                let text = error_text(&result.unwrap_err());
                assert!(text.contains(expected), "{label}: {text}");
                assert!(fixture.core.terminal("t7").is_some(), "{label}");
                fixture.assert_retained(label);
            }
            None => {
                assert!(
                    matches!(result, Ok(CloseOutcome::Missing)),
                    "{label}: {:?}",
                    result.map_err(|error| error_text(&error))
                );
                fixture.assert_closed(label);
            }
        }
    }
}

// Earlier TAB versions confirmed the launch label and generated name at creation but
// did not save the nonce in the surface record. Keep those records usable on upgrade.
#[test]
fn ended_close_checks_the_launch_label_with_current_and_legacy_records() {
    for (legacy, changed_label) in [
        (false, Some("0123456789abcdef01234567deadbeef")),
        (true, Some("000000000000000000000000deadbeef")),
        (true, None),
    ] {
        let fixture = Fixture::new();
        fixture.finish();
        if legacy {
            let store = fixture.store();
            let record = store.record(CoreRecord::Terminal);
            let mut value = serde_json::to_value(fixture.store().terminal().unwrap()).unwrap();
            assert!(
                value
                    .as_object_mut()
                    .unwrap()
                    .remove("herdr_launch_nonce")
                    .is_some()
            );
            record.write_json(&value).unwrap();
        }
        if let Some(nonce) = changed_label {
            let mut labels = fixture.core.terminal("t7").unwrap()["labels"].clone();
            labels[LAUNCH_LABEL] = json!(nonce);
            fixture.core.set("t7", "labels", labels);
            assert!(
                fixture.close_dead().is_err(),
                "removed a terminal with a changed launch label"
            );
            assert_eq!(fixture.mutations(), (0, 0));
            fixture.assert_retained("launch label mismatch");
        } else {
            fixture.close_dead().unwrap();
            fixture.assert_closed("legacy launch");
        }
    }
}

// A removal is attempted once per close. A refused removal keeps the handle and the next close
// removes the terminal; a removal whose reply was lost is not repeated blindly, and the next
// close finds the terminal absent.
#[test]
fn a_failed_or_unanswered_removal_is_finished_by_the_next_close() {
    let fixture = Fixture::new();
    fixture.finish();
    fixture
        .core
        .inject("terminal.rm", Injected::Instead(refused("internal")));
    let text = error_text(&fixture.close_dead().unwrap_err());
    assert!(
        text.contains("Herdr refused terminal.rm (internal)"),
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 1));
    assert!(fixture.core.terminal("t7").is_some());
    fixture.assert_retained("refused removal");
    fixture.core.clear_calls();
    assert!(matches!(fixture.close_dead(), Ok(CloseOutcome::Closed)));
    assert_eq!(fixture.mutations(), (0, 1));
    assert!(fixture.core.terminal("t7").is_none());
    fixture.assert_closed("retried removal");

    let fixture = Fixture::new();
    fixture.finish();
    fixture.core.inject(
        "terminal.rm",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&fixture.close_dead().unwrap_err());
    assert!(text.contains("sent but not answered"), "{text}");
    assert!(fixture.core.terminal("t7").is_none());
    fixture.assert_retained("unanswered removal");
    fixture.core.clear_calls();
    assert!(matches!(fixture.close_dead(), Ok(CloseOutcome::Missing)));
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.assert_closed("close after an unanswered removal");
}
