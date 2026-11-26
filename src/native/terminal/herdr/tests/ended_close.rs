//! #399: the Herdr terminal ended outside TAB (its tab or pane was closed, `hc kill`, or its
//! program exited) and took the native-session owner with it. An explicit close runs the
//! session's real close records and decision over the scripted core; only the OS observation
//! of the dead owner is injected. The rule for every gone owner and terminal state, on every
//! OS, is in `owner_gone`.
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
        .with_posix_dead_owner_observations(
            &adapters,
            &|_| gone(Gone::Exited),
            &|_| false,
            &|_| panic!("an exited owner needs no reuse corroboration"),
        );
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
                .with_posix_dead_owner_observations(&adapters, &observe, &pid_is_alive, &|_| true)
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
    // The decision asks the core nothing; the close proves the terminal by its creation
    // record, reads it again before acting, removes it once without a kill, and confirms
    // that the core no longer holds it.
    assert_eq!(
        fixture.core.methods(),
        [
            "connect",
            "hello",
            "terminal.get",
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

// A Herdr terminal is closed with its live owner or on its creation record, never on the
// surface alone: that authority is refused before anything is sent.
#[test]
fn a_herdr_terminal_is_never_closed_on_the_surface_alone() {
    let fixture = Fixture::new();
    let surface = fixture.store().terminal().unwrap();
    let error = posix::close_owned_session(
        &Adapter::with_core(fixture.core.clone()),
        &fixture.directory,
        SESSION,
        &surface,
        posix::OwnedSurfaceClose::SurfaceOnly,
        session::close::budget::close_deadline(),
    )
    .unwrap_err();
    assert!(
        error_text(&error).contains("closed with its live owner or on its creation record"),
        "{error:#}"
    );
    assert_eq!(fixture.mutations(), (0, 0));
}
