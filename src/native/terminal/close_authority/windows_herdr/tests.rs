use super::*;
use crate::native::{
    clock::FakeClock,
    session::{CoreRecord, Store, close::budget, schema::ProviderProcessRecord},
};
use anyhow::bail;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

const WRAPPER: u32 = 41;
const PROVIDER: u32 = 42;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    owner: NativeSessionOwner,
    calls: RefCell<Vec<(u32, &'static str)>>,
    provider_exited: Cell<bool>,
    failure: Cell<&'static str>,
    now: Rc<Cell<Instant>>,
    deadline: Instant,
    _clock: budget::Scope,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let session_dir = directory.path().join("session-test");
        std::fs::create_dir(&session_dir).unwrap();
        let store = Store::open_unchecked(&session_dir);
        let owner = NativeSessionOwner {
            pid: WRAPPER,
            managed_session_id: Some("session-test".into()),
            windows_process_identity: Some(identity(WRAPPER)),
            ..Default::default()
        };
        store.write_owner(&owner).unwrap();
        store.write_provider_process(&provider_record()).unwrap();
        let start = Instant::now();
        let clock = FakeClock::new(start);
        Self {
            _directory: directory,
            store,
            owner,
            calls: RefCell::new(Vec::new()),
            provider_exited: Cell::new(false),
            failure: Cell::new(""),
            now: clock.shared(),
            deadline: start + Duration::from_secs(10),
            _clock: budget::Scope::with_clock(clock),
        }
    }

    fn close(&self, present: impl FnOnce() -> Result<bool>) -> Result<CloseResult> {
        teardown(
            &self.store,
            &self.owner,
            present,
            |pid, recorded| {
                self.calls.borrow_mut().push((pid, "pin"));
                ensure!(
                    recorded == &identity(pid),
                    "recorded birth or image differs"
                );
                match (pid, self.failure.get()) {
                    (WRAPPER, "wrapper denied") | (PROVIDER, "provider denied") => {
                        bail!("access denied")
                    }
                    (PROVIDER, _) if self.provider_exited.get() => Ok(None),
                    _ => Ok(Some(Process { pid, fixture: self })),
                }
            },
            self.deadline,
        )
    }
}

fn identity(pid: u32) -> WindowsProcessIdentity {
    WindowsProcessIdentity {
        creation_time: u64::from(pid),
        executable_path: format!("test-{pid}.exe"),
    }
}

fn provider_record() -> ProviderProcessRecord {
    ProviderProcessRecord {
        schema: 1,
        pid: PROVIDER,
        managed_session_id: "session-test".into(),
        windows_process_identity: Some(identity(PROVIDER)),
        spawned_unix_ms: 10,
    }
}

struct Process<'a> {
    pid: u32,
    fixture: &'a Fixture,
}
impl PinnedProcess for Process<'_> {
    fn exited(&self) -> Result<bool> {
        Ok(self.pid == WRAPPER && self.fixture.failure.get() == "wrapper exited")
    }
    fn stop(&self, _: Instant) -> Result<bool> {
        self.fixture.calls.borrow_mut().push((self.pid, "stop"));
        match (self.pid, self.fixture.failure.get()) {
            (WRAPPER, "wrapper stop") | (PROVIDER, "provider stop") => bail!("stop denied"),
            (WRAPPER, "wrapper confirmation") => bail!("confirmation failed"),
            (PROVIDER, "expire") => self.fixture.now.set(self.fixture.deadline),
            _ => {}
        }
        if self.pid == PROVIDER {
            self.fixture.provider_exited.set(true);
        }
        Ok(true)
    }
}

#[test]
fn both_pins_precede_either_stop_and_wrapper_failure_reports_the_confirmed_provider() {
    for failure in ["wrapper stop", "wrapper confirmation", "expire"] {
        let fixture = Fixture::new();
        fixture.failure.set(failure);
        let result = fixture.close(|| Ok(false)).unwrap_err();
        assert_eq!(
            result.to_string(),
            "recorded provider PID 42 was already ended by TAB; could not confirm exit of recorded wrapper PID 41; the handle is retained"
        );
        let mut expected = vec![(WRAPPER, "pin"), (PROVIDER, "pin"), (PROVIDER, "stop")];
        if failure != "expire" {
            expected.push((WRAPPER, "stop"));
        }
        assert_eq!(*fixture.calls.borrow(), expected);
        fixture.calls.borrow_mut().clear();
        fixture.failure.set("");
        fixture.now.set(fixture.deadline - Duration::from_secs(10));
        assert_eq!(
            fixture.close(|| Ok(false)).unwrap().reason.as_deref(),
            Some(
                "Herdr terminal absent; recorded provider PID 42 had already exited; recorded wrapper PID 41 was ended by TAB"
            )
        );
        assert_eq!(
            *fixture.calls.borrow(),
            [(WRAPPER, "pin"), (PROVIDER, "pin"), (WRAPPER, "stop")]
        );
    }
}

#[test]
fn absent_unreadable_foreign_changed_or_unpinnable_provider_ends_only_the_wrapper() {
    for case in [
        "absent",
        "unreadable",
        "foreign",
        "mismatch",
        "provider denied",
        "changed",
    ] {
        let fixture = Fixture::new();
        let mut record = provider_record();
        let file = fixture.store.record(CoreRecord::ProviderProcess);
        match case {
            "absent" => std::fs::remove_file(file.path()).unwrap(),
            "unreadable" => std::fs::write(file.path(), [0xff]).unwrap(),
            "foreign" => {
                record.managed_session_id = "session-other".into();
                fixture.store.write_provider_process(&record).unwrap();
            }
            "mismatch" => {
                record.windows_process_identity = Some(identity(77));
                fixture.store.write_provider_process(&record).unwrap();
            }
            "provider denied" => fixture.failure.set(case),
            _ => {}
        }
        let result = fixture
            .close(|| {
                if case == "changed" {
                    record.pid = 77;
                    fixture.store.write_provider_process(&record)?;
                }
                Ok(false)
            })
            .unwrap();
        assert!(
            result
                .reason
                .unwrap()
                .starts_with("Herdr terminal absent; no provider process was ended by TAB: ")
        );
        let mut expected = vec![(WRAPPER, "pin")];
        if matches!(case, "mismatch" | "provider denied" | "changed") {
            expected.push((PROVIDER, "pin"));
        }
        expected.push((WRAPPER, "stop"));
        assert_eq!(*fixture.calls.borrow(), expected, "{case}");
    }
}

#[test]
fn absent_herdr_requires_the_same_absence_and_live_wrapper_before_the_first_stop() {
    for case in [
        "present",
        "core unavailable",
        "wrapper changed",
        "wrapper exited",
        "wrapper denied",
        "provider stop",
    ] {
        let fixture = Fixture::new();
        fixture.failure.set(case);
        assert!(
            fixture
                .close(|| {
                    if case == "core unavailable" {
                        bail!("core lost");
                    }
                    if case == "wrapper changed" {
                        let mut replacement = fixture.owner.clone();
                        replacement.windows_process_identity = Some(identity(87));
                        fixture.store.write_owner(&replacement)?;
                    }
                    Ok(case == "present")
                })
                .is_err()
        );
        let mut expected = vec![(WRAPPER, "pin")];
        if case != "wrapper denied" {
            expected.push((PROVIDER, "pin"));
        }
        if case == "provider stop" {
            expected.push((PROVIDER, "stop"));
        }
        assert_eq!(*fixture.calls.borrow(), expected, "{case}");
    }
}

#[cfg(windows)]
#[test]
fn partial_windows_close_restores_the_handle_and_retry_skips_the_exited_provider() {
    use crate::native::{
        process_identity::{Attested, ProcessObservation},
        session::{
            self,
            owner::{ControlAttestation, OwnerAttestation},
        },
        terminal::{TerminalKind, TerminalSession},
    };
    for failure in ["wrapper stop", "wrapper confirmation", "expire"] {
        let fixture = Fixture::new();
        let store = &fixture.store;
        session::test_support::lifecycle::write_owned_terminal_state(
            store.directory(),
            "ready",
            WRAPPER,
        );
        store.write_owner(&fixture.owner).unwrap();
        let handle = store.record(CoreRecord::Terminal);
        handle
            .write_json(
                &TerminalSession::new(TerminalKind::Herdr, "t7".into())
                    .with_binding(Some("session-test".into())),
            )
            .unwrap();
        let initial = handle.bytes().unwrap().unwrap();
        let run = || {
            session::close::close_with_attestation(
                store,
                None,
                fixture.deadline,
                |_| OwnerAttestation {
                    process: ProcessObservation::Live(Attested::Incarnation),
                    control: ControlAttestation::NotRecorded,
                    pid_alive: Some(true),
                },
                |_| fixture.close(|| Ok(false)),
            )
        };
        fixture.failure.set(failure);
        let error = run().unwrap_err();
        assert_eq!(
            error.to_string(),
            "recorded provider PID 42 was already ended by TAB; could not confirm exit of recorded wrapper PID 41; the handle is retained"
        );
        assert_eq!(handle.bytes().unwrap().unwrap(), initial);
        assert!(!store.record(CoreRecord::TerminalClosed).present().unwrap());
        fixture.failure.set("");
        fixture.now.set(fixture.deadline - Duration::from_secs(10));
        fixture.calls.borrow_mut().clear();
        assert_eq!(run().unwrap(), CloseOutcome::Missing);
        assert_eq!(
            *fixture.calls.borrow(),
            [(WRAPPER, "pin"), (PROVIDER, "pin"), (WRAPPER, "stop")]
        );
        assert!(store.terminal_closed().unwrap().consumed);
    }
}
