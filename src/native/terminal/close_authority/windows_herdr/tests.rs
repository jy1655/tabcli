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
                if recorded.creation_time != identity(pid).creation_time {
                    return Ok(ProcessPin::Reused);
                }
                ensure!(
                    recorded.executable_path == identity(pid).executable_path,
                    "recorded Windows process {pid} has a different executable at the same creation time"
                );
                match (pid, self.failure.get()) {
                    (WRAPPER, "wrapper denied") | (PROVIDER, "provider denied") => {
                        bail!("access denied")
                    }
                    (WRAPPER, "exited at pin") => Ok(ProcessPin::Exited),
                    (PROVIDER, _) if self.provider_exited.get() => Ok(ProcessPin::Exited),
                    _ => Ok(ProcessPin::Live(Process { pid, fixture: self })),
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
            (WRAPPER, "expire after wrapper") => self.fixture.now.set(self.fixture.deadline),
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
fn missing_unreadable_foreign_or_changed_provider_record_ends_only_the_wrapper() {
    for case in [
        "absent",
        "unreadable",
        "foreign",
        "reused",
        "incomplete birth",
        "incomplete image",
        "no identity",
        "invalid pid",
        "wrapper pid",
        "unsupported schema",
        "changed",
    ] {
        let fixture = Fixture::new();
        let mut record = provider_record();
        let file = fixture.store.record(CoreRecord::ProviderProcess);
        match case {
            "absent" => std::fs::remove_file(file.path()).unwrap(),
            "unreadable" => std::fs::write(file.path(), [0xff]).unwrap(),
            _ => {
                let windows_identity = record.windows_process_identity.as_mut().unwrap();
                match case {
                    "foreign" => record.managed_session_id = "session-other".into(),
                    "reused" => windows_identity.creation_time += 1,
                    "incomplete birth" => windows_identity.creation_time = 0,
                    "incomplete image" => windows_identity.executable_path.clear(),
                    "no identity" => record.windows_process_identity = None,
                    "invalid pid" => record.pid = 0,
                    "wrapper pid" => record.pid = WRAPPER,
                    "unsupported schema" => record.schema = 9,
                    "changed" => {}
                    _ => unreachable!(),
                }
                fixture.store.write_provider_process(&record).unwrap();
            }
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
        let record_error;
        let cause = match case {
            "incomplete birth" | "incomplete image" => {
                "the recorded provider has an incomplete Windows identity"
            }
            "no identity" => "the recorded provider has no Windows identity",
            "invalid pid" | "wrapper pid" => {
                "the provider process record has an invalid provider PID"
            }
            "changed" => "the recorded provider identity changed",
            "foreign" | "unsupported schema" => {
                "the provider process record is unsupported or belongs to another session"
            }
            "absent" => "the provider process record is missing",
            "unreadable" => {
                record_error = format!(
                    "{:#}",
                    fixture.store.provider_process_if_present().unwrap_err()
                );
                &record_error
            }
            "reused" => "",
            _ => unreachable!(),
        };
        let skip_reason = if case == "reused" {
            "recorded provider PID 42 was reused; no provider process was ended by TAB".to_owned()
        } else {
            format!("no provider process was ended by TAB: {cause}")
        };
        assert_eq!(
            result.reason.unwrap(),
            format!(
                "Herdr terminal absent; {skip_reason}; recorded wrapper PID 41 was ended by TAB"
            )
        );
        let mut expected = vec![(WRAPPER, "pin")];
        if matches!(case, "reused" | "changed") {
            expected.push((PROVIDER, "pin"));
        }
        expected.push((WRAPPER, "stop"));
        assert_eq!(*fixture.calls.borrow(), expected, "{case}");
    }
}

#[test]
fn a_recorded_provider_inspection_failure_retains_the_handle_and_stops_neither_process() {
    for cause in [
        "access denied",
        "recorded Windows process 42 has a different executable at the same creation time",
    ] {
        let fixture = Fixture::new();
        if cause == "access denied" {
            fixture.failure.set("provider denied");
        } else {
            let mut record = provider_record();
            record
                .windows_process_identity
                .as_mut()
                .unwrap()
                .executable_path
                .push_str(".other");
            fixture.store.write_provider_process(&record).unwrap();
        }
        let error = fixture.close(|| Ok(false)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!("could not verify recorded provider PID 42; the handle is retained: {cause}")
        );
        assert_eq!(
            *fixture.calls.borrow(),
            [(WRAPPER, "pin"), (PROVIDER, "pin")]
        );
    }
}

#[test]
fn a_wrapper_pin_failure_names_the_wrapper_and_retains_the_handle() {
    let fixture = Fixture::new();
    fixture.failure.set("wrapper denied");
    let error = fixture.close(|| Ok(false)).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "could not verify recorded wrapper PID 41; the handle is retained: access denied"
    );
    assert_eq!(*fixture.calls.borrow(), [(WRAPPER, "pin")]);
}

#[test]
fn an_exited_or_reused_wrapper_refuses_before_the_provider_pin() {
    for (state, expected) in [
        (
            "exited at pin",
            "recorded wrapper PID 41 has exited; the handle is retained; retry close",
        ),
        (
            "reused",
            "recorded wrapper PID 41 was reused; the handle is retained; retry close",
        ),
    ] {
        let mut fixture = Fixture::new();
        if state == "reused" {
            fixture
                .owner
                .windows_process_identity
                .as_mut()
                .unwrap()
                .creation_time += 1;
            fixture.store.write_owner(&fixture.owner).unwrap();
        } else {
            fixture.failure.set(state);
        }
        let error = fixture
            .close(|| panic!("wrapper pin must refuse before presence"))
            .unwrap_err();
        assert_eq!(format!("{error:#}"), expected);
        assert_eq!(*fixture.calls.borrow(), [(WRAPPER, "pin")]);
    }
}

#[test]
fn expiry_after_both_stops_reports_the_confirmed_provider_and_wrapper() {
    let fixture = Fixture::new();
    fixture.failure.set("expire after wrapper");
    let error = fixture.close(|| Ok(false)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Herdr terminal absent; recorded provider PID 42 was already ended by TAB; recorded wrapper PID 41 was ended by TAB; the handle is retained"
    );
    assert_eq!(
        error.root_cause().to_string(),
        budget::close_remaining(fixture.deadline)
            .unwrap_err()
            .to_string()
    );
    assert_eq!(
        *fixture.calls.borrow(),
        [
            (WRAPPER, "pin"),
            (PROVIDER, "pin"),
            (PROVIDER, "stop"),
            (WRAPPER, "stop")
        ]
    );
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
fn partial_windows_close_keeps_the_handle_and_retry_skips_the_exited_provider() {
    use crate::native::{
        process_identity::{Attested, ProcessObservation},
        session::{
            self,
            owner::{ControlAttestation, OwnerAttestation},
        },
        terminal::{TerminalKind, TerminalSession},
    };
    for failure in [
        "provider denied",
        "wrapper stop",
        "wrapper confirmation",
        "expire",
        "expire after wrapper",
        "expire after adapter",
        "expire at marker write",
        "expire after marker write",
    ] {
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
                |_| {
                    let result = fixture.close(|| Ok(false))?;
                    if fixture.failure.get() == "expire after adapter" {
                        fixture.now.set(fixture.deadline);
                    }
                    Ok(result)
                },
            )
        };
        fixture.failure.set(failure);
        if matches!(
            failure,
            "expire at marker write" | "expire after marker write"
        ) {
            let now = fixture.now.clone();
            let deadline = fixture.deadline;
            // The marker is the first atomic write after the adapter confirms both stops.
            let published = failure == "expire after marker write";
            let step = if published {
                "syncing a renamed record"
            } else {
                "renaming a temporary record over its final path"
            };
            let error = session::primitives::with_step_at(step, move || now.set(deadline), run)
                .unwrap_err();
            let reason = "Herdr terminal absent; recorded provider PID 42 was already ended by TAB; recorded wrapper PID 41 was ended by TAB";
            let expiry = budget::close_remaining(deadline).unwrap_err().to_string();
            assert_eq!(error.root_cause().to_string(), expiry);
            if published {
                assert_eq!(error.to_string(), expiry);
                let marker = store.terminal_closed().unwrap();
                assert!(marker.consumed);
                assert_eq!(marker.reason.as_deref(), Some(reason));
            } else {
                assert_eq!(
                    error.to_string(),
                    format!("{reason}; the claimed handle is kept for a retry")
                );
                assert!(!store.record(CoreRecord::TerminalClosed).present().unwrap());
            }
            assert_eq!(
                *fixture.calls.borrow(),
                [
                    (WRAPPER, "pin"),
                    (PROVIDER, "pin"),
                    (PROVIDER, "stop"),
                    (WRAPPER, "stop")
                ]
            );
            assert!(handle.bytes().unwrap().is_none());
            assert_eq!(
                store
                    .record(CoreRecord::TerminalClosing)
                    .bytes()
                    .unwrap()
                    .unwrap(),
                initial
            );
            continue;
        }
        let error = run().unwrap_err();
        assert_eq!(
            error.to_string(),
            match failure {
                "provider denied" =>
                    "could not verify recorded provider PID 42; the handle is retained",
                "expire after wrapper" | "expire after adapter" =>
                    "Herdr terminal absent; recorded provider PID 42 was already ended by TAB; recorded wrapper PID 41 was ended by TAB; the handle is retained",
                _ =>
                    "recorded provider PID 42 was already ended by TAB; could not confirm exit of recorded wrapper PID 41; the handle is retained",
            }
        );
        assert_eq!(handle.bytes().unwrap().unwrap(), initial);
        assert!(!store.record(CoreRecord::TerminalClosed).present().unwrap());
        if matches!(failure, "expire after wrapper" | "expire after adapter") {
            continue; // The wrapper is gone. A real retry must acquire fresh owner authority.
        }
        fixture.failure.set("");
        fixture.now.set(fixture.deadline - Duration::from_secs(10));
        fixture.calls.borrow_mut().clear();
        assert_eq!(run().unwrap(), CloseOutcome::Missing);
        let mut expected = vec![(WRAPPER, "pin"), (PROVIDER, "pin")];
        if failure == "provider denied" {
            expected.push((PROVIDER, "stop"));
        }
        expected.push((WRAPPER, "stop"));
        assert_eq!(*fixture.calls.borrow(), expected);
        assert!(store.terminal_closed().unwrap().consumed);
    }
}
