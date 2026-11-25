//! A Herdr terminal whose native-session owner is verified gone (os-parity, the owner's
//! decision of 2026-11-26): on every OS, Repair leaves it for an explicit close, and the
//! explicit close ends and removes it on the terminal's own creation record, whatever the
//! owner record says (exited, PID reused, unbound, another session's, incomplete). These
//! tests run the session's real Records, Repair and close transaction over the scripted
//! core; only the OS observation of the owner is injected. One expected table holds on
//! every OS; each OS runs its own close wiring.
use super::*;
use crate::native::process_identity::{Gone, ProcessObservation};
use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
use crate::native::session::test_support::lifecycle::write_owned_terminal_state;
use crate::native::session::{self, CoreRecord, SessionState, Store};
use crate::native::terminal::ownership::NativeSessionOwner;

/// The owner record as the wrapper inside the terminal wrote it: bound to the session, with
/// its whole recorded identity on this platform.
fn complete_owner() -> NativeSessionOwner {
    NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        #[cfg(unix)]
        terminal_tty: Some("/dev/ttys042".into()),
        #[cfg(unix)]
        terminal_tty_device: Some(42),
        #[cfg(unix)]
        process_group: Some(4242),
        #[cfg(unix)]
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
        #[cfg(windows)]
        windows_process_identity: Some(crate::native::terminal::WindowsProcessIdentity {
            creation_time: 134_000_000_000_000_042,
            executable_path: r"C:\tabcli\tabcli.exe".into(),
        }),
        ..NativeSessionOwner::default()
    }
}

/// Every owner record a gone owner can leave. None of them may decide the close.
fn owner_records() -> Vec<(&'static str, NativeSessionOwner)> {
    let bound = complete_owner();
    let mut unbound = complete_owner();
    unbound.managed_session_id = None;
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let incomplete = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        ..NativeSessionOwner::default()
    };
    vec![
        ("bound", bound),
        ("unbound", unbound),
        ("another session's", foreign),
        ("incomplete identity", incomplete),
    ]
}

fn gone(gone: Gone) -> OwnerAttestation {
    OwnerAttestation {
        process: ProcessObservation::Gone(gone),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(false),
    }
}

fn exited() -> OwnerAttestation {
    gone(Gone::Exited)
}

fn reused() -> OwnerAttestation {
    gone(Gone::Reused("different birth".into()))
}

fn unknown() -> OwnerAttestation {
    OwnerAttestation {
        process: ProcessObservation::Unknown("owner inspection failed".into()),
        control: ControlAttestation::NotRecorded,
        pid_alive: None,
    }
}

type Observe = fn() -> OwnerAttestation;
/// A close's expected answer: its outcome, or a fragment of its refusal.
type Expected = Result<CloseOutcome, &'static str>;
/// The `terminal.kill` and `terminal.rm` calls a close sends.
type Mutations = (usize, usize);

/// The ways the owner is observed gone. `pid_alive` is the macOS and Linux PID probe the
/// ordinary dead-owner rule also consults; it decides nothing for a Herdr terminal.
const GONE: [(&str, Observe, bool); 3] = [
    ("exited", exited, false),
    ("exited, PID taken again", exited, true),
    ("PID reused", reused, false),
];

struct Fixture {
    _root: tempfile::TempDir,
    directory: PathBuf,
    core: FakeCore,
    console: FakeCore,
}

impl Fixture {
    /// A session in `state` whose live Herdr terminal `t7` TAB created on the scripted core,
    /// with `owner` as its owner record and the turn claim its state implies.
    fn new(state: &str, owner: &NativeSessionOwner) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(SESSION);
        fs::create_dir(&directory).unwrap();
        write_owned_terminal_state(&directory, state, owner.pid);
        let core = FakeCore::new();
        let surface = opened(&core, &directory);
        let store = Store::open_unchecked(&directory);
        store.write_owner(owner).unwrap();
        store
            .record(CoreRecord::Terminal)
            .write_json(&surface)
            .unwrap();
        core.clear_calls();
        Self {
            _root: root,
            directory,
            core,
            console: FakeCore::new(),
        }
    }

    fn store(&self) -> Store {
        Store::open_unchecked(&self.directory)
    }

    fn finish(&self) {
        self.core.set("t7", "state", json!("finished"));
    }

    fn remove(&self) {
        self.core.0.borrow_mut().terminals.remove("t7");
    }

    /// An interrupted close left the handle claimed.
    fn claim(&self) {
        self.store().claim_terminal_handle().unwrap().unwrap();
    }

    /// Everything Repair must leave as it found it.
    fn records(&self) -> Vec<Option<Vec<u8>>> {
        let store = self.store();
        [
            CoreRecord::Terminal,
            CoreRecord::TerminalClosing,
            CoreRecord::TerminalClosed,
            CoreRecord::TurnClaim,
            CoreRecord::Owner,
            CoreRecord::Status,
            CoreRecord::Closed,
        ]
        .into_iter()
        .map(|record| store.record(record).bytes().unwrap())
        .collect()
    }

    fn mutations(&self) -> (usize, usize) {
        (
            self.core.params("terminal.kill").len(),
            self.core.params("terminal.rm").len(),
        )
    }

    /// Repair under every platform policy this build compiles: the Windows one everywhere,
    /// and the macOS and Linux one on those systems.
    fn repair(&self, observe: Observe) -> Vec<Result<bool>> {
        let store = self.store();
        let dispatch = crate::native::terminal::windows_dispatch::Dispatch::new(
            Adapter::with_core(self.console.clone()),
            Adapter::with_core(self.core.clone()),
        );
        #[allow(unused_mut)]
        let mut results = vec![session::close::repair_dead_owner_windows_with(
            &store,
            |_| observe(),
            &dispatch,
            later(),
        )];
        #[cfg(unix)]
        results.push(session::close::repair_dead_owner_attested_until(
            &store,
            |_| observe(),
            later(),
        ));
        results
    }

    /// `tabcli close-session --explicit` through this OS's close wiring, with the owner
    /// observed as `observe`. The scripted observation refuses any owner proof.
    fn close(&self, observe: Observe, pid_alive: bool) -> Result<CloseOutcome> {
        let deadline = session::close::budget::close_deadline();
        let attest = |_: &NativeSessionOwner| observe();
        #[cfg(unix)]
        {
            use crate::native::terminal::close_authority::Production;
            let pid_is_alive = move |_| pid_alive;
            let adapters = Adapter::with_core(self.core.clone());
            session::close::close_with_attestation(
                &self.store(),
                None,
                deadline,
                attest,
                |surface| {
                    Production::new(&self.directory, SESSION, surface, deadline)
                        .with_posix_dead_owner_observations(&adapters, &attest, &pid_is_alive)
                        .close_posix_surface()
                },
            )
        }
        #[cfg(windows)]
        {
            use crate::native::terminal::close_authority::{
                close_windows_owned_surface_with, decide, test_support::TestObservation,
            };
            let _ = pid_alive;
            let dispatch = crate::native::terminal::windows_dispatch::Dispatch::new(
                Adapter::with_core(self.console.clone()),
                Adapter::with_core(self.core.clone()),
            );
            session::close::close_with_attestation(
                &self.store(),
                None,
                deadline,
                attest,
                |surface| {
                    let observation =
                        TestObservation::on_records(&self.directory, SESSION, surface, deadline)
                            .with_owner_record(move |_| observe())
                            .with_presence(|| panic!("a gone owner's close asks no presence"))
                            // The installed proof refuses an owner that is not live; a gone
                            // owner's close never asks for it.
                            .with_proof(|| Err(anyhow!("the owner is not proven live (scripted)")));
                    close_windows_owned_surface_with(
                        || decide(&observation, SESSION, surface, deadline),
                        &dispatch,
                        &self.directory,
                        surface,
                        deadline,
                    )
                },
            )
        }
    }

    fn assert_closed(&self, label: &str) {
        let store = self.store();
        assert!(
            store
                .record(CoreRecord::Terminal)
                .bytes()
                .unwrap()
                .is_none(),
            "{label}: the handle must be consumed"
        );
        assert!(
            store
                .record(CoreRecord::TerminalClosing)
                .bytes()
                .unwrap()
                .is_none(),
            "{label}: the claimed handle must be consumed"
        );
        assert!(
            store.record(CoreRecord::TerminalClosed).present().unwrap(),
            "{label}: the close must leave its tombstone"
        );
        assert_eq!(
            store.status().unwrap().state,
            SessionState::Closed,
            "{label}"
        );
        assert!(self.core.terminal("t7").is_none(), "{label}");
    }

    fn assert_retained(&self, label: &str, state: SessionState) {
        let store = self.store();
        assert!(
            store.record(CoreRecord::Terminal).present().unwrap(),
            "{label}: the handle must be retained"
        );
        assert!(
            !store.record(CoreRecord::TerminalClosed).present().unwrap(),
            "{label}"
        );
        assert_eq!(store.status().unwrap().state, state, "{label}");
    }
}

// Repair never ends, removes or even observes the terminal of a gone owner, on any OS and in
// any repairable state, whatever the owner record says and whether the handle is active or
// claimed by an interrupted close. It leaves every Record as it found it.
#[test]
fn repair_leaves_a_gone_owners_terminal_for_an_explicit_close() {
    for state in ["ready", "working", "exited", "failed"] {
        for (record, owner) in owner_records() {
            for claimed in [false, true] {
                for (how, observe, _) in GONE {
                    let label = format!("{state}, {record} owner {how}, claimed {claimed}");
                    let fixture = Fixture::new(state, &owner);
                    if claimed {
                        fixture.claim();
                    }
                    let before = fixture.records();
                    for result in fixture.repair(observe) {
                        assert!(
                            !result.unwrap_or_else(|error| panic!("{label}: {error:#}")),
                            "{label}"
                        );
                        assert!(
                            fixture.core.methods().is_empty(),
                            "{label}: {:?}",
                            fixture.core.methods()
                        );
                        assert!(fixture.console.methods().is_empty(), "{label}");
                        assert_eq!(fixture.records(), before, "{label}");
                    }
                    assert_eq!(fixture.core.terminal("t7").unwrap()["state"], "live");
                }
            }
        }
    }
}

// Nothing is stranded: for every gone owner, whatever its record, the explicit close ends a
// running terminal once, removes a finished one, settles an absent one, and closes the
// session; a repeated close asks the core nothing.
#[test]
fn an_explicit_close_ends_the_terminal_its_session_created_whatever_the_owner_record_says() {
    type Terminal = fn(&Fixture);
    let terminals: [(&str, Terminal, Option<CloseOutcome>, Mutations); 3] = [
        ("running", |_| {}, Some(CloseOutcome::Closed), (1, 1)),
        (
            "finished",
            Fixture::finish,
            Some(CloseOutcome::Closed),
            (0, 1),
        ),
        (
            "absent",
            Fixture::remove,
            Some(CloseOutcome::Missing),
            (0, 0),
        ),
    ];
    for state in ["ready", "exited"] {
        for (record, owner) in owner_records() {
            for (how, observe, pid_alive) in GONE {
                for (terminal, change, outcome, mutations) in terminals {
                    for claimed in [false, true] {
                        let label = format!(
                            "{state}, {record} owner {how}, {terminal} terminal, claimed {claimed}"
                        );
                        let fixture = Fixture::new(state, &owner);
                        change(&fixture);
                        if claimed {
                            fixture.claim();
                        }
                        let result = fixture.close(observe, pid_alive);
                        assert_eq!(
                            result.as_ref().ok(),
                            outcome.as_ref(),
                            "{label}: {:?}",
                            result.as_ref().map_err(error_text)
                        );
                        assert_eq!(fixture.mutations(), mutations, "{label}");
                        fixture.assert_closed(&label);
                        fixture.core.clear_calls();
                        assert!(fixture.close(observe, pid_alive).is_ok(), "{label}");
                        assert!(
                            fixture.core.methods().is_empty(),
                            "{label}: {:?}",
                            fixture.core.methods()
                        );
                    }
                }
            }
        }
    }
}

/// One refusal of a gone owner's close, and what the user does about it before closing again.
struct Refusal {
    label: &'static str,
    change: fn(&Fixture),
    observe: Observe,
    refusal: &'static str,
    /// What the user does after reading the refusal: remove a terminal they have checked with
    /// `hc rm`, turn keep off, or bring the recorded core back.
    recover: fn(&Fixture),
    /// How the owner is observed at the next close.
    again: Observe,
    closed: CloseOutcome,
}

fn another_label(f: &Fixture) {
    let mut labels = f.core.terminal("t7").unwrap()["labels"].clone();
    labels[LAUNCH_LABEL] = json!("fedcba9876543210fedcba9876543210");
    f.core.set("t7", "labels", labels);
}

// The creation record decides, and only it: a terminal that is not provably the one this
// session created, or that a replacement program could run in, is left alone with its handle,
// on every OS, and an owner that cannot be inspected is never taken for gone. Nothing is
// stranded: once the user has dealt with what the refusal names, the next close finishes.
#[test]
fn an_explicit_close_leaves_anything_but_its_created_terminal_alone() {
    let rows = [
        Refusal {
            label: "finished and kept",
            change: |f| {
                f.finish();
                f.core.set("t7", "keep", json!(true));
            },
            observe: exited,
            refusal: "set to keep",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "running and kept",
            change: |f| f.core.set("t7", "keep", json!(true)),
            observe: reused,
            refusal: "set to keep",
            recover: |f| f.core.set("t7", "keep", json!(false)),
            again: reused,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "another program",
            change: |f| f.core.set("t7", "pid", json!(4242)),
            observe: exited,
            refusal: "not the terminal this session created",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another program incarnation",
            change: |f| {
                let pid = f.store().terminal().unwrap().herdr_core().unwrap().host_pid;
                f.core.0.borrow_mut().births.insert(pid, test_birth(1));
            },
            observe: reused,
            refusal: "another process than the one recorded",
            recover: Fixture::remove,
            again: reused,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another creation",
            change: |f| f.core.set("t7", "created_ms", json!(1)),
            observe: exited,
            refusal: "not the terminal this session created",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another session's directory",
            change: |f| {
                let mut argv = f.core.terminal("t7").unwrap()["argv"].clone();
                argv[2] = json!("/state/session-other");
                f.core.set("t7", "argv", argv);
            },
            observe: reused,
            refusal: "not the terminal this session created",
            recover: Fixture::remove,
            again: reused,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another launch label, running",
            change: another_label,
            observe: exited,
            refusal: "different launch label",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another launch label, finished",
            change: |f| {
                f.finish();
                another_label(f);
            },
            observe: reused,
            refusal: "different launch label",
            recover: Fixture::remove,
            again: reused,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "another core",
            change: |f| f.core.0.borrow_mut().core_id = "core-b".into(),
            observe: exited,
            refusal: "a different Herdr Core (id core-b)",
            recover: |f| f.core.0.borrow_mut().core_id = "core-a".into(),
            again: exited,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "no core",
            change: |f| f.core.0.borrow_mut().running = false,
            observe: reused,
            refusal: "no Herdr Core is running",
            recover: |f| f.core.0.borrow_mut().running = true,
            again: reused,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "owner not inspectable",
            change: |_| {},
            observe: unknown,
            #[cfg(unix)]
            refusal: "owner inspection failed",
            #[cfg(windows)]
            refusal: "the owner is not proven live (scripted)",
            recover: |_| {},
            again: exited,
            closed: CloseOutcome::Closed,
        },
    ];
    for row in rows {
        let label = row.label;
        let fixture = Fixture::new("ready", &complete_owner());
        (row.change)(&fixture);
        let before = fixture.core.terminal("t7");
        fixture.core.clear_calls();
        let text = error_text(&fixture.close(row.observe, false).unwrap_err());
        assert!(text.contains(row.refusal), "{label}: {text}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        assert_eq!(fixture.core.terminal("t7"), before, "{label}");
        fixture.assert_retained(label, SessionState::Ready);

        (row.recover)(&fixture);
        let result = fixture.close(row.again, false);
        assert_eq!(
            result.as_ref().ok(),
            Some(&row.closed),
            "{label}: {:?}",
            result.as_ref().map_err(error_text)
        );
        fixture.assert_closed(label);
    }
}

// Every read before an action is checked again against the creation record: a terminal that
// another client of the same user changes after the first read is left alone, one that runs
// again is still this session's and is ended once, and one already gone is absent.
#[test]
fn a_terminal_that_changes_between_reads_is_checked_again() {
    type Change = fn(&mut BTreeMap<String, Value>);
    let rows: [(&str, Change, Expected, Mutations); 4] = [
        (
            "kept",
            |terminals| terminals.get_mut("t7").unwrap()["keep"] = json!(true),
            Err("set to keep"),
            (0, 0),
        ),
        (
            "replacement program",
            |terminals| {
                let terminal = terminals.get_mut("t7").unwrap();
                terminal["state"] = json!("live");
                terminal["pid"] = json!(4242);
            },
            Err("not the terminal this session created"),
            (0, 0),
        ),
        (
            "running again",
            |terminals| terminals.get_mut("t7").unwrap()["state"] = json!("live"),
            Ok(CloseOutcome::Closed),
            (1, 1),
        ),
        (
            "removed by someone else",
            |terminals| {
                terminals.remove("t7");
            },
            Ok(CloseOutcome::Missing),
            (0, 0),
        ),
    ];
    for (label, change, expected, mutations) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        fixture.finish();
        // The first read proves the terminal; the change lands before the next read.
        fixture.core.after_call("terminal.get", change);
        let result = fixture.close(reused, false);
        assert_eq!(fixture.mutations(), mutations, "{label}");
        match expected {
            Ok(outcome) => {
                assert_eq!(
                    result.as_ref().ok(),
                    Some(&outcome),
                    "{label}: {:?}",
                    result.as_ref().map_err(error_text)
                );
                fixture.assert_closed(label);
            }
            Err(text) => {
                let error = error_text(&result.unwrap_err());
                assert!(error.contains(text), "{label}: {error}");
                assert!(fixture.core.terminal("t7").is_some(), "{label}");
                fixture.assert_retained(label, SessionState::Ready);
            }
        }
    }
}

// A close is attempted once per command. A refused removal keeps the handle and the next close
// removes the terminal; a removal whose reply was lost is not repeated blindly, and the next
// close finds the terminal absent. The owner stays gone throughout, PID reused.
#[test]
fn a_failed_or_unanswered_removal_is_finished_by_the_next_close() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    fixture
        .core
        .inject("terminal.rm", Injected::Instead(refused("internal")));
    let text = error_text(&fixture.close(reused, false).unwrap_err());
    assert!(
        text.contains("Herdr refused terminal.rm (internal)"),
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 1));
    assert!(fixture.core.terminal("t7").is_some());
    fixture.assert_retained("refused removal", SessionState::Ready);
    fixture.core.clear_calls();
    assert!(matches!(
        fixture.close(reused, false),
        Ok(CloseOutcome::Closed)
    ));
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_closed("retried removal");

    let fixture = Fixture::new("ready", &complete_owner());
    fixture.core.inject(
        "terminal.kill",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains("sent but not answered"), "{text}");
    assert_eq!(fixture.core.terminal("t7").unwrap()["state"], "finished");
    fixture.assert_retained("unanswered kill", SessionState::Ready);
    fixture.core.clear_calls();
    assert!(matches!(
        fixture.close(exited, false),
        Ok(CloseOutcome::Closed)
    ));
    // The finished terminal is only removed: the kill is never sent twice.
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_closed("close after an unanswered kill");
}

// The Windows lookup, compiled on every OS: a gone owner's authority reaches the Herdr
// adapter's close by creation record, a kind without one is refused before any adapter, and
// a proven absence reaches none.
#[test]
fn the_windows_lookup_closes_a_gone_owners_terminal_by_its_creation_record() {
    use crate::native::terminal::close_authority::{
        TerminalCloseAuthority, close_windows_owned_surface_with,
    };
    use crate::native::terminal::windows_dispatch::Dispatch;
    type Terminal = fn(&Fixture);
    let rows: [(&str, Terminal, Expected, Mutations); 4] = [
        ("running", |_| {}, Ok(CloseOutcome::Closed), (1, 1)),
        (
            "finished",
            Fixture::finish,
            Ok(CloseOutcome::Closed),
            (0, 1),
        ),
        ("absent", Fixture::remove, Ok(CloseOutcome::Missing), (0, 0)),
        (
            "another launch label",
            |f| {
                let mut labels = f.core.terminal("t7").unwrap()["labels"].clone();
                labels[LAUNCH_LABEL] = json!("fedcba9876543210fedcba9876543210");
                f.core.set("t7", "labels", labels);
            },
            Err("different launch label"),
            (0, 0),
        ),
    ];
    for (label, change, expected, mutations) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        change(&fixture);
        let surface = fixture.store().terminal().unwrap();
        let dispatch = Dispatch::new(
            Adapter::with_core(fixture.console.clone()),
            Adapter::with_core(fixture.core.clone()),
        );
        let result = close_windows_owned_surface_with(
            || Ok(TerminalCloseAuthority::CreatedSurface),
            &dispatch,
            &fixture.directory,
            &surface,
            later(),
        );
        match expected {
            Ok(outcome) => assert_eq!(result.unwrap().outcome, outcome, "{label}"),
            Err(text) => {
                let error = error_text(&result.unwrap_err());
                assert!(error.contains(text), "{label}: {error}");
            }
        }
        assert_eq!(fixture.mutations(), mutations, "{label}");
        assert!(fixture.console.methods().is_empty(), "{label}");
    }

    let fixture = Fixture::new("ready", &complete_owner());
    let dispatch = Dispatch::new(
        Adapter::with_core(fixture.console.clone()),
        Adapter::with_core(fixture.core.clone()),
    );
    let mut console = fixture.store().terminal().unwrap();
    console.kind = TerminalKind::WindowsConsole;
    let error = close_windows_owned_surface_with(
        || Ok(TerminalCloseAuthority::CreatedSurface),
        &dispatch,
        &fixture.directory,
        &console,
        later(),
    )
    .unwrap_err();
    assert!(
        error_text(&error).contains("keeps no creation record"),
        "{error:#}"
    );
    let surface = fixture.store().terminal().unwrap();
    assert_eq!(
        close_windows_owned_surface_with(
            || Ok(TerminalCloseAuthority::Absent),
            &dispatch,
            &fixture.directory,
            &surface,
            later(),
        )
        .unwrap()
        .outcome,
        CloseOutcome::Missing
    );
    assert!(fixture.core.methods().is_empty());
    assert!(fixture.console.methods().is_empty());
}
