//! A Herdr terminal whose native-session owner is gone (os-parity, the owner's decisions of
//! 2026-11-26): on every OS, Repair leaves it for an explicit close, and the explicit close
//! proves it by its own creation record, whatever the owner record says. That close removes a
//! finished terminal and settles an absent one, and never ends a running one: it refuses, and
//! once the person has ended the terminal or it has finished, the same command removes it. These tests run the
//! session's real Records, Repair and close transaction over the scripted core; only the OS
//! observation of the owner is injected. One expected table holds on every OS; each OS runs
//! its own close wiring.
use super::*;
use crate::native::process_identity::Attested;
use crate::native::process_identity::{Gone, ProcessObservation};
use crate::native::session::owner::{ControlAttestation, OwnerAttestation};
use crate::native::session::test_support::lifecycle::write_owned_terminal_state;
use crate::native::session::{self, CoreRecord, SessionState, Store};
use crate::native::terminal::close_authority::CloseCause;
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

/// Every owner record a gone owner can leave. None of them lets a close end a running terminal,
/// and none of them blocks the close of a finished or absent one.
fn owner_records() -> Vec<(&'static str, NativeSessionOwner)> {
    let bound = complete_owner();
    let mut unbound = complete_owner();
    unbound.managed_session_id = None;
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let pid_only = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        ..NativeSessionOwner::default()
    };
    vec![
        ("bound", bound),
        ("unbound", unbound),
        ("another session's", foreign),
        ("PID-only", pid_only),
    ]
}

/// The refusal of a close on the creation record for a running terminal.
const STILL_RUNNING: &str =
    "is still running, and TAB could not verify a live owner for this session";

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

/// A live process with its recorded incarnation: another session's owner, when the record
/// is not bound to this session.
fn live_other() -> OwnerAttestation {
    OwnerAttestation {
        process: ProcessObservation::Live(Attested::Incarnation),
        control: ControlAttestation::Verified,
        pid_alive: Some(true),
    }
}

/// A live process at the PID of a record that names no incarnation, as after PID recycling.
fn live_pid_only() -> OwnerAttestation {
    OwnerAttestation {
        process: ProcessObservation::Live(Attested::PidOnly),
        control: ControlAttestation::NotRecorded,
        pid_alive: Some(true),
    }
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

    /// A session whose owner record is the one `write_owned_terminal_state` writes for `pid`,
    /// so that the OS observes it for real, with the Herdr terminal as its handle.
    fn observed(state: &str, pid: u32) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(SESSION);
        fs::create_dir(&directory).unwrap();
        write_owned_terminal_state(&directory, state, pid);
        let core = FakeCore::new();
        let surface = opened(&core, &directory);
        Store::open_unchecked(&directory)
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

    /// The stored state keeps a diagnosis, as a provider failure or a launch failure leaves.
    fn diagnose(&self, state: SessionState, error: &str) {
        self.store()
            .write_status(&crate::native::session::SessionStatus {
                state,
                generation: 7,
                updated_unix_ms: 1,
                exit_code: Some(1),
                error: Some(error.into()),
                residual_surface: None,
            })
            .unwrap();
    }

    /// A launch that failed before its provider was confirmed: status `failed` and a launch
    /// receipt that never reached `spawned`.
    fn failed_start(&self) {
        let store = self.store();
        self.diagnose(SessionState::Failed, "provider launch timed out");
        store
            .record(CoreRecord::Launch)
            .write_json(&crate::native::session::launch::Record {
                schema: 1,
                claim_token: "claim-test".into(),
                deadline_unix_ms: 1,
                phase: crate::native::session::launch::Phase::Spawning,
            })
            .unwrap();
    }

    fn relabel(&self) {
        let mut labels = self.core.terminal("t7").unwrap()["labels"].clone();
        labels[LAUNCH_LABEL] = json!("fedcba9876543210fedcba9876543210");
        self.core.set("t7", "labels", labels);
    }

    /// The OS no longer reports the terminal's program, although the core still lists the
    /// terminal as running: its birth cannot be observed.
    fn lose_program(&self) {
        let pid = self
            .store()
            .terminal()
            .unwrap()
            .herdr_core()
            .unwrap()
            .host_pid;
        self.core.0.borrow_mut().births.remove(&pid);
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
        self.close_as(observe, pid_alive, CloseCause::Explicit)
    }

    /// The same close for `cause`: a cleanup after another command instead of
    /// `close-session --explicit`.
    fn close_as(
        &self,
        observe: Observe,
        pid_alive: bool,
        cause: CloseCause,
    ) -> Result<CloseOutcome> {
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
                        .for_cause(cause)
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
                            .with_cause(cause)
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

// The dead-owner decision in Repair never ends, removes or even observes the terminal of a
// gone owner, on any OS and in any repairable state, whatever the owner record says and
// whether the handle is active or claimed by an interrupted close. With no completion to
// publish and no launch to settle, Repair leaves every Record as it found it.
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

// Nothing is stranded, and nothing running is ended (the owner's decision of 2026-11-26). For
// every gone owner, whatever its record and whether the session recorded its own end, the
// explicit close removes a finished terminal, settles an absent one and closes the session; a
// repeated close asks the core nothing. A running terminal is retained and nothing is sent;
// once it has finished, or the person has ended it on its recorded core, the same command
// removes it. One table on every OS, through each OS's close wiring.
#[test]
fn an_explicit_close_settles_the_terminal_its_session_created_and_never_ends_it() {
    type Terminal = fn(&Fixture);
    let terminals: [(&str, Terminal); 3] = [
        ("running", |_| {}),
        ("finished", Fixture::finish),
        ("absent", Fixture::remove),
    ];
    for state in ["ready", "exited"] {
        for (record, owner) in owner_records() {
            for (how, observe, pid_alive) in GONE {
                for (terminal, change) in terminals {
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
                        if terminal == "running" {
                            let text = error_text(result.as_ref().unwrap_err());
                            assert!(text.contains(STILL_RUNNING), "{label}: {text}");
                            assert_eq!(fixture.mutations(), (0, 0), "{label}");
                            assert_eq!(fixture.core.terminal("t7").unwrap()["state"], "live");
                            fixture.assert_retained(&label, state.parse().unwrap());
                            // The person ends it on its recorded core; the next close removes it.
                            fixture.finish();
                            fixture.core.clear_calls();
                            assert_eq!(
                                fixture
                                    .close(observe, pid_alive)
                                    .map_err(|e| error_text(&e)),
                                Ok(CloseOutcome::Closed),
                                "{label}"
                            );
                            assert_eq!(fixture.mutations(), (0, 1), "{label}");
                        } else {
                            let (outcome, mutations) = if terminal == "finished" {
                                (CloseOutcome::Closed, (0, 1))
                            } else {
                                (CloseOutcome::Missing, (0, 0))
                            };
                            assert_eq!(
                                result.as_ref().ok(),
                                Some(&outcome),
                                "{label}: {:?}",
                                result.as_ref().map_err(error_text)
                            );
                            assert_eq!(fixture.mutations(), mutations, "{label}");
                        }
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
            recover: |f| {
                f.core.set("t7", "keep", json!(false));
                f.finish();
            },
            again: reused,
            closed: CloseOutcome::Closed,
        },
        // A running terminal is never ended on its creation record, whatever is known about
        // the session's owner: the person waits for it to finish or ends it on its recorded
        // core, and the next close removes it.
        Refusal {
            label: "running, owner exited",
            change: |_| {},
            observe: exited,
            refusal: STILL_RUNNING,
            recover: Fixture::finish,
            again: exited,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "running, owner PID reused",
            change: |_| {},
            observe: reused,
            refusal: STILL_RUNNING,
            recover: Fixture::finish,
            again: reused,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "running, the session recorded its own end",
            change: |f| f.diagnose(SessionState::Exited, "provider exited with status 0"),
            observe: exited,
            refusal: STILL_RUNNING,
            recover: Fixture::finish,
            again: exited,
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
            change: |f| {
                f.finish();
                f.core.0.borrow_mut().core_id = "core-b".into();
            },
            observe: exited,
            refusal: "a different Herdr Core (id core-b)",
            recover: |f| f.core.0.borrow_mut().core_id = "core-a".into(),
            again: exited,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "no core",
            change: |f| {
                f.finish();
                f.core.0.borrow_mut().running = false;
            },
            observe: reused,
            refusal: "no Herdr Core is running",
            recover: |f| f.core.0.borrow_mut().running = true,
            again: reused,
            closed: CloseOutcome::Closed,
        },
        Refusal {
            label: "running, its program not observable",
            change: Fixture::lose_program,
            observe: reused,
            refusal: STILL_RUNNING,
            recover: Fixture::finish,
            again: reused,
            closed: CloseOutcome::Closed,
        },
        // A failed start whose owner is gone, and one that never recorded an owner, close on
        // the same creation record, launch label included.
        Refusal {
            label: "failed start, another launch label, running",
            change: |f| {
                f.failed_start();
                f.relabel();
            },
            observe: exited,
            refusal: "different launch label",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "failed start, another launch label, finished",
            change: |f| {
                f.failed_start();
                f.finish();
                f.relabel();
            },
            observe: reused,
            refusal: "different launch label",
            recover: Fixture::remove,
            again: reused,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "failed start without an owner, another launch label",
            change: |f| {
                f.failed_start();
                f.finish();
                f.relabel();
                fs::remove_file(f.store().record(CoreRecord::Owner).path()).unwrap();
            },
            observe: exited,
            refusal: "different launch label",
            recover: Fixture::remove,
            again: exited,
            closed: CloseOutcome::Missing,
        },
        Refusal {
            label: "failed start, running",
            change: Fixture::failed_start,
            observe: exited,
            refusal: STILL_RUNNING,
            recover: Fixture::finish,
            again: exited,
            closed: CloseOutcome::Closed,
        },
    ];
    for row in rows {
        let label = row.label;
        let fixture = Fixture::new("ready", &complete_owner());
        (row.change)(&fixture);
        let state = fixture.store().status().unwrap().state;
        let before = fixture.core.terminal("t7");
        fixture.core.clear_calls();
        let text = error_text(&fixture.close(row.observe, false).unwrap_err());
        assert!(text.contains(row.refusal), "{label}: {text}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        assert_eq!(fixture.core.terminal("t7"), before, "{label}");
        fixture.assert_retained(label, state);

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
// again is still this session's and is not ended, and one already gone is absent.
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
            Err(STILL_RUNNING),
            (0, 0),
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

    // A removal whose reply was lost: the core removed the terminal, the close cannot know it
    // and keeps the handle; the next close finds it absent and sends nothing.
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    fixture.core.inject(
        "terminal.rm",
        Injected::After(CallError::Unanswered(anyhow!("Herdr reply timed out"))),
    );
    let text = error_text(&fixture.close(reused, false).unwrap_err());
    assert!(text.contains("sent but not answered"), "{text}");
    assert!(fixture.core.terminal("t7").is_none());
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_retained("unanswered removal", SessionState::Ready);
    fixture.core.clear_calls();
    assert!(matches!(
        fixture.close(reused, false),
        Ok(CloseOutcome::Missing)
    ));
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.assert_closed("close after an unanswered removal");

    // With an owner record that cannot be read, a refusal after the
    // first read keeps its own error and, once, why TAB could not verify a live owner: a refused
    // second read, a refused removal, and a confirming read that still finds the terminal. The
    // next close removes the finished terminal.
    type Script = fn(&Fixture);
    let rows: [(&str, Script, &str, Mutations); 3] = [
        (
            "second read refused",
            |fixture| {
                fixture.core.inject("terminal.get", Injected::Pass);
                fixture
                    .core
                    .inject("terminal.get", Injected::Instead(refused("internal")));
            },
            "Herdr refused terminal.get (internal)",
            (0, 0),
        ),
        (
            "removal refused",
            |fixture| {
                fixture
                    .core
                    .inject("terminal.rm", Injected::Instead(refused("internal")));
            },
            "Herdr refused terminal.rm (internal)",
            (0, 1),
        ),
        (
            "still listed after the removal",
            |fixture| {
                let terminal = fixture.core.terminal("t7").unwrap();
                fixture.core.after_call("terminal.rm", move |terminals| {
                    terminals.insert("t7".into(), terminal);
                });
            },
            "is still held by its core after terminal.rm",
            (0, 1),
        ),
    ];
    for (label, script, refusal, mutations) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        fixture.finish();
        let owner = fixture.store().record(CoreRecord::Owner).path().to_owned();
        fs::remove_file(&owner).unwrap();
        fs::create_dir(&owner).unwrap();
        script(&fixture);
        let text = error_text(&fixture.close(exited, false).unwrap_err());
        assert!(text.contains(refusal), "{label}: {text}");
        assert!(
            text.contains(
                "TAB could not verify a live owner for this session (its owner record could not be read"
            ),
            "{label}: {text}"
        );
        assert_eq!(
            text.matches("its owner record could not be read").count(),
            1,
            "{label}: {text}"
        );
        assert_eq!(fixture.mutations(), mutations, "{label}");
        fixture.assert_retained(label, SessionState::Ready);
        fixture.core.clear_calls();
        assert_eq!(
            fixture.close(exited, false).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Closed),
            "{label}"
        );
        assert_eq!(fixture.mutations(), (0, 1), "{label}");
        fixture.assert_closed(label);
    }
}

// Every Windows Herdr handle TAB has written (0.4.47 on) records its launch label, so one
// without it cannot be proven TAB's and is never ended or removed. The
// name fallback stays for the macOS and Linux handles of 0.4.45 and 0.4.46. Runs on the
// Windows runner only.
#[cfg(windows)]
#[test]
fn a_windows_handle_without_its_launch_label_is_refused() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let store = fixture.store();
    let mut value = serde_json::to_value(store.terminal().unwrap()).unwrap();
    assert!(
        value
            .as_object_mut()
            .unwrap()
            .remove("herdr_launch_nonce")
            .is_some()
    );
    store
        .record(CoreRecord::Terminal)
        .write_json(&value)
        .unwrap();
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains("records no launch label"), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.assert_retained("Windows handle without a launch label", SessionState::Ready);
}

// One removal and one confirmation. `not_found` from the recorded core
// answers the removal itself: the terminal is absent and the close asks nothing more. A
// removal the core acknowledged while the next read still lists the terminal is not sent
// again: the handle is kept for the next close.
#[test]
fn a_removal_is_sent_once_and_confirmed_once() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    fixture
        .core
        .inject("terminal.rm", Injected::Instead(refused("not_found")));
    fixture.core.clear_calls();
    assert_eq!(
        fixture.close(reused, false).map_err(|e| error_text(&e)),
        Ok(CloseOutcome::Missing)
    );
    assert_eq!(fixture.mutations(), (0, 1));
    assert_eq!(
        fixture.core.methods().last().map(String::as_str),
        Some("terminal.rm"),
        "{:?}",
        fixture.core.methods()
    );
    let store = fixture.store();
    assert!(store.record(CoreRecord::TerminalClosed).present().unwrap());
    assert_eq!(store.status().unwrap().state, SessionState::Closed);

    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let held = fixture.core.terminal("t7").unwrap();
    fixture.core.after_call("terminal.rm", move |terminals| {
        terminals.insert("t7".into(), held);
    });
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains("still held by its core"), "{text}");
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_retained("acknowledged removal still listed", SessionState::Ready);
}

// A Herdr close keeps the diagnosis the session already holds: the closed record carries the
// stored failure read under the lifecycle lock.
#[test]
fn an_explicit_close_keeps_the_sessions_diagnosis() {
    let fixture = Fixture::new("exited", &complete_owner());
    fixture.finish();
    fixture.diagnose(SessionState::Exited, "provider exited with status 7");
    fixture.close(exited, false).unwrap();
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(
        closed.error.as_deref(),
        Some("provider exited with status 7")
    );

    // Without a stored diagnosis, an owner observed gone records its generated reason, as
    // 0.4.49's Repair did.
    let fixture = Fixture::new("working", &complete_owner());
    fixture.finish();
    fixture.close(exited, false).unwrap();
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(
        closed.error.as_deref(),
        Some("native session process 4242 is no longer running")
    );

    // A close that only removes a finished terminal, with nothing establishing this session's
    // end, adds no reason of its own.
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let fixture = Fixture::new("working", &foreign);
    fixture.finish();
    fixture.close(exited, false).unwrap();
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(closed.error, None);
}

// Repair scans the active and the claimed handle without the lifecycle lock, while an explicit
// close holds that lock from its claim to its restore. A close that refuses and restores its
// handle between the scan's two reads leaves both reads empty; Repair must then find the
// handle again under the lock before it settles the session, or the restored handle is
// consumed and the terminal is stranded (0.4.49 had the same interleaving on
// macOS and Linux, and on Windows in exited and failed sessions). Repair takes the lifecycle
// lock first to recover a pending completion, then scans; the hook restores the handle at the
// next lifecycle lock, the first one after the scan.
#[test]
fn repair_finds_a_restored_handle_again_under_the_lifecycle_lock() {
    use crate::native::session::primitives::close_io_test::{self, Point};
    use std::{cell::Cell, rc::Rc};
    type Repair = fn(&Fixture) -> Result<bool>;
    #[cfg_attr(windows, allow(unused_mut))]
    let mut policies: Vec<(&str, Repair)> = vec![("windows", |f| {
        let dispatch = crate::native::terminal::windows_dispatch::Dispatch::new(
            Adapter::with_core(f.console.clone()),
            Adapter::with_core(f.core.clone()),
        );
        session::close::repair_dead_owner_windows_with(&f.store(), |_| exited(), &dispatch, later())
    })];
    #[cfg(unix)]
    policies.push(("posix", |f| {
        session::close::repair_dead_owner_attested_until(&f.store(), |_| exited(), later())
    }));
    for (policy, repair) in policies {
        for state in ["exited", "failed"] {
            let label = format!("{policy}, {state}");
            let fixture = Fixture::new(state, &complete_owner());
            let handle = fixture
                .store()
                .record(CoreRecord::Terminal)
                .path()
                .to_owned();
            let held = fixture.directory.with_file_name("held-by-a-close.json");
            fs::rename(&handle, &held).unwrap();
            let locks = Rc::new(Cell::new(0));
            let seen = locks.clone();
            let (from, to) = (held.clone(), handle.clone());
            let result = close_io_test::during(
                move |point| {
                    if point == Point::BeforeLock("turn claim lifecycle") {
                        seen.set(seen.get() + 1);
                        if seen.get() == 2 {
                            fs::rename(&from, &to).unwrap();
                        }
                    }
                },
                || repair(&fixture),
            );
            assert!(
                locks.get() >= 2,
                "{label}: no lifecycle lock after the scan ({} in all)",
                locks.get()
            );
            assert!(
                !result.unwrap_or_else(|error| panic!("{label}: {error:#}")),
                "{label}"
            );
            fixture.assert_retained(&label, state.parse().unwrap());
            assert!(fixture.core.methods().is_empty(), "{label}");
        }
    }
}

// An attached `ask` or `tell` returns as soon as the owner is gone, on every OS, without
// changing a Record: Repair now leaves the terminal on Windows too, so the
// wait no longer learns of the owner's death from a closed session. The owner is a reaped
// process that the OS observes for real.
#[test]
fn an_attached_wait_returns_once_the_owner_is_gone_on_every_os() {
    let fixture = Fixture::observed(
        "working",
        crate::native::session::test_support::lifecycle::reaped_child_pid(),
    );
    let before = fixture.records();
    let requested = Duration::from_secs(20);
    let started = std::time::Instant::now();
    let error = session::turn::wait(
        &fixture.store(),
        0,
        None,
        None,
        started + requested,
        requested,
    )
    .unwrap_err();
    assert!(
        error_text(&error).contains("native session process"),
        "{error:#}"
    );
    let error = session::turn::wait_for_status(
        &fixture.directory,
        SessionState::Ready,
        std::time::Instant::now() + requested,
        requested,
    )
    .unwrap_err();
    assert!(
        error_text(&error).contains("native session process"),
        "{error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.records(), before);
    assert!(fixture.core.methods().is_empty());
}

// The Windows lookup, compiled on every OS: a gone owner's authority reaches the Herdr
// adapter's close by creation record, a kind without one is refused before any adapter, and
// a proven absence reaches none.
#[test]
fn the_windows_lookup_closes_a_gone_owners_terminal_by_its_creation_record() {
    use crate::native::terminal::close_authority::{
        CreationEvidence, TerminalCloseAuthority, close_windows_owned_surface_with,
    };
    use crate::native::terminal::windows_dispatch::Dispatch;
    type Terminal = fn(&Fixture);
    let rows: [(&str, Terminal, Expected, Mutations); 4] = [
        ("running", |_| {}, Err(STILL_RUNNING), (0, 0)),
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
            || {
                Ok(TerminalCloseAuthority::CreatedSurface(
                    CreationEvidence::scripted(CloseCause::Explicit),
                ))
            },
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
        || {
            Ok(TerminalCloseAuthority::CreatedSurface(
                CreationEvidence::scripted(CloseCause::Explicit),
            ))
        },
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
    // A terminal its core keeps is never closed on the surface alone: that decision closes
    // on the creation record with the decision's own result, which keeps no diagnosis. A running
    // terminal is refused with nothing sent; a finished one is removed.
    let surface_only = || {
        close_windows_owned_surface_with(
            || Ok(TerminalCloseAuthority::SurfaceOnly),
            &dispatch,
            &fixture.directory,
            &surface,
            later(),
        )
    };
    let text = error_text(&surface_only().unwrap_err());
    assert!(text.contains(STILL_RUNNING), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.finish();
    let result = surface_only().unwrap();
    assert_eq!(result.outcome, CloseOutcome::Closed);
    assert!(!result.keeps_diagnosis);
    assert_eq!(fixture.mutations(), (0, 1));
    assert!(fixture.console.methods().is_empty());
}

// A record that names a live process which is not this session's
// owner (another session's owner, or a recycled PID in a PID-only record) never strands the
// session. The running terminal is retained; once it has been ended on its recorded core,
// the same command removes it while that other process stays alive, and an absent terminal
// is settled.
#[test]
fn a_live_process_that_is_not_this_sessions_owner_never_strands_the_session() {
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let mut unbound = complete_owner();
    unbound.managed_session_id = None;
    let pid_only = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        ..NativeSessionOwner::default()
    };
    let rows: [(&str, NativeSessionOwner, Observe); 3] = [
        ("another session's live owner", foreign, live_other),
        ("an unbound record of a live process", unbound, live_other),
        (
            "a PID-only record whose PID was recycled",
            pid_only,
            live_pid_only,
        ),
    ];
    for (label, owner, observe) in rows {
        let fixture = Fixture::new("working", &owner);
        let text = error_text(&fixture.close(observe, true).unwrap_err());
        assert!(text.contains(STILL_RUNNING), "{label}: {text}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        fixture.assert_retained(label, SessionState::Working);
        fixture.finish();
        fixture.core.clear_calls();
        assert_eq!(
            fixture.close(observe, true).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Closed),
            "{label}"
        );
        assert_eq!(fixture.mutations(), (0, 1), "{label}");
        fixture.assert_closed(label);

        let fixture = Fixture::new("working", &owner);
        fixture.remove();
        assert_eq!(
            fixture.close(observe, true).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Missing),
            "{label}, absent"
        );
        fixture.assert_closed(label);
    }
}

// Under the owner's decision of 2026-11-26: a session whose own
// wrapper recorded its end (`exited`) still keeps its running terminal, whatever the owner
// record says; once the terminal has finished, the same command removes it.
#[test]
fn a_session_that_recorded_its_own_end_still_keeps_its_running_terminal() {
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let pid_only = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some(SESSION.into()),
        ..NativeSessionOwner::default()
    };
    let rows: [(&str, NativeSessionOwner, Observe); 4] = [
        ("this session's gone owner", complete_owner(), exited),
        ("another session's live owner", foreign.clone(), live_other),
        ("another session's gone owner", foreign, exited),
        (
            "a PID-only record whose PID was recycled",
            pid_only,
            live_pid_only,
        ),
    ];
    for (label, owner, observe) in rows {
        let fixture = Fixture::new("exited", &owner);
        let text = error_text(&fixture.close(observe, false).unwrap_err());
        assert!(text.contains(STILL_RUNNING), "{label}: {text}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        fixture.assert_retained(label, SessionState::Exited);
        fixture.finish();
        fixture.core.clear_calls();
        assert_eq!(
            fixture.close(observe, false).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Closed),
            "{label}"
        );
        assert_eq!(fixture.mutations(), (0, 1), "{label}");
        fixture.assert_closed(label);
    }
}

// A cleanup after another command (a refused reopen) never ends or
// removes a terminal that no live owner holds, the failed start included, and settles no
// absent one: it sends nothing and keeps the handle and the state.
#[test]
fn a_cleanup_never_acts_on_a_terminal_without_a_live_owner() {
    type Change = fn(&Fixture);
    let rows: [(&str, Change); 5] = [
        ("failed start, running", Fixture::failed_start),
        ("failed start, finished", |f| {
            f.failed_start();
            f.finish();
        }),
        ("gone owner, running", |_| {}),
        ("gone owner, finished", Fixture::finish),
        ("gone owner, absent", Fixture::remove),
    ];
    for (label, change) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        change(&fixture);
        let state = fixture.store().status().unwrap().state;
        let before = fixture.core.terminal("t7");
        fixture.core.clear_calls();
        assert!(
            fixture
                .close_as(exited, false, CloseCause::Cleanup)
                .is_err(),
            "{label}"
        );
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        assert_eq!(fixture.core.terminal("t7"), before, "{label}");
        fixture.assert_retained(label, state);
    }
}

// A session without an owner record may have a wrapper that is
// registering, so its running terminal is kept; a finished one is removed and an absent one
// settled.
#[test]
fn a_session_without_an_owner_record_keeps_its_running_terminal() {
    let forget = |f: &Fixture| {
        f.failed_start();
        fs::remove_file(f.store().record(CoreRecord::Owner).path()).unwrap();
    };
    let fixture = Fixture::new("ready", &complete_owner());
    forget(&fixture);
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains(STILL_RUNNING), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.assert_retained("no owner record, running", SessionState::Failed);
    fixture.finish();
    assert_eq!(
        fixture.close(exited, false).map_err(|e| error_text(&e)),
        Ok(CloseOutcome::Closed)
    );
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_closed("no owner record, finished");

    let fixture = Fixture::new("ready", &complete_owner());
    forget(&fixture);
    fixture.remove();
    assert_eq!(
        fixture.close(exited, false).map_err(|e| error_text(&e)),
        Ok(CloseOutcome::Missing)
    );
    fixture.assert_closed("no owner record, absent");
}

/// A handle damage for the absence test.
type Damage = fn(&mut serde_json::Map<String, Value>);

// A close settles an absent terminal from its
// creation record when the decoded host PID, host birth and launch label pass the checks derived
// from the released writers. Each accepted row is one released writer's output, as its tag's
// source writes it:
// 0.4.45 (macOS) and 0.4.46 (macOS, Linux) omit the launch label; 0.4.47 to 0.4.49 (macOS,
// Linux, Windows) save the 32 lowercase hexadecimal digits their generator wrote; the host PID is
// one the core reported and the platform's reader returned a birth for; the host birth is the
// platform's own kind (0.4.45's flat macOS fields are the same wire). Each refused row damages
// one field, and the refusal names that field before the core's
// `not_found` can settle the handle.
#[test]
fn a_creation_record_is_whole_exactly_when_a_released_writer_could_have_written_it() {
    const PID: &str = "records a host PID that this platform's process reader never reports";
    const BIRTH: &str = "records a host birth that no writer on this platform records";
    const LABEL: &str = "records a damaged launch label";
    fn omit_label(handle: &mut serde_json::Map<String, Value>) {
        handle.remove("herdr_launch_nonce");
    }
    fn as_written(_: &mut serde_json::Map<String, Value>) {}
    // The host birth fields each platform's writers record.
    #[cfg(target_os = "macos")]
    const BIRTH_FIELDS: &[&str] = &["host_start_microseconds", "host_start_seconds"];
    #[cfg(target_os = "linux")]
    const BIRTH_FIELDS: &[&str] = &["linux_birth"];
    #[cfg(windows)]
    const BIRTH_FIELDS: &[&str] = &["host_creation_time"];
    // Whether each release saved `herdr_launch_nonce`, from its tag's `record.rs`: 0.4.45 and
    // 0.4.46 have no such field; 0.4.47 to 0.4.49 set it in `with_herdr_core`.
    #[allow(unused_mut)]
    let mut accepted: Vec<(&str, Damage, bool)> = Vec::new();
    #[cfg(target_os = "macos")]
    accepted.extend([
        ("0.4.45, macOS", omit_label as Damage, false),
        ("0.4.46, macOS", omit_label, false),
        ("0.4.47, macOS", as_written, true),
        ("0.4.48, macOS", as_written, true),
        ("0.4.49, macOS", as_written, true),
    ]);
    #[cfg(target_os = "linux")]
    accepted.extend([
        ("0.4.46, Linux", omit_label as Damage, false),
        ("0.4.47, Linux", as_written, true),
        ("0.4.48, Linux", as_written, true),
        ("0.4.49, Linux", as_written, true),
        // procfs reports zero ticks for a program started in its time namespace's first tick,
        // and the writers copied it.
        (
            "0.4.46, Linux, started at tick zero",
            |handle| {
                omit_label(handle);
                handle.get_mut("herdr_core").unwrap()["linux_birth"]["starttime_ticks"] = json!(0);
            },
            false,
        ),
        (
            "0.4.49, Linux, started at tick zero",
            |handle| {
                handle.get_mut("herdr_core").unwrap()["linux_birth"]["starttime_ticks"] = json!(0);
            },
            true,
        ),
    ]);
    #[cfg(windows)]
    accepted.extend([
        ("0.4.47, Windows", as_written as Damage, true),
        ("0.4.48, Windows", as_written, true),
        ("0.4.49, Windows", as_written, true),
    ]);
    for (label, written, saves_label) in accepted {
        let fixture = Fixture::new("ready", &complete_owner());
        let store = fixture.store();
        let mut value = serde_json::to_value(store.terminal().unwrap()).unwrap();
        written(value.as_object_mut().unwrap());
        // The fields that writer saved, and no others.
        let mut top: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top.sort_unstable();
        let mut expected_top = vec!["herdr_core", "managed_session_id", "session_id", "terminal"];
        if saves_label {
            expected_top.push("herdr_launch_nonce");
            expected_top.sort_unstable();
        }
        assert_eq!(top, expected_top, "{label}");
        let mut core: Vec<&str> = value["herdr_core"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        core.sort_unstable();
        let mut expected_core = vec!["core_id", "created_ms", "host_pid", "socket"];
        expected_core.extend(BIRTH_FIELDS);
        expected_core.sort_unstable();
        assert_eq!(core, expected_core, "{label}");
        store
            .record(CoreRecord::Terminal)
            .write_json(&value)
            .unwrap();
        fixture.remove();
        assert_eq!(
            fixture.close(exited, false).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Missing),
            "{label}"
        );
        fixture.assert_closed(label);
    }

    let mut refused: Vec<(&str, Damage, &str)> = vec![
        (
            "host PID zero",
            |handle| handle.get_mut("herdr_core").unwrap()["host_pid"] = json!(0),
            PID,
        ),
        (
            "a launch label TAB never generates",
            |handle| {
                handle.insert("herdr_launch_nonce".into(), json!("not-a-launch-label"));
            },
            LABEL,
        ),
        (
            "a launch label of 24 digits",
            |handle| {
                handle.insert(
                    "herdr_launch_nonce".into(),
                    json!("0123456789abcdef01234567"),
                );
            },
            LABEL,
        ),
        (
            "an upper-case launch label",
            |handle| {
                handle.insert(
                    "herdr_launch_nonce".into(),
                    json!("0123456789ABCDEF0123456789ABCDEF"),
                );
            },
            LABEL,
        ),
        (
            "an explicit null launch label",
            |handle| {
                handle.insert("herdr_launch_nonce".into(), Value::Null);
            },
            "records a null launch label",
        ),
    ];
    // The macOS and Linux readers never report a PID above `i32::MAX`.
    #[cfg(unix)]
    refused.extend([
        (
            "host PID above i32::MAX",
            (|handle| handle.get_mut("herdr_core").unwrap()["host_pid"] = json!(2_147_483_648_u64))
                as Damage,
            PID,
        ),
        (
            "host PID 4294967295",
            |handle| handle.get_mut("herdr_core").unwrap()["host_pid"] = json!(u32::MAX),
            PID,
        ),
        (
            "another platform's host birth",
            |handle| {
                let core = handle
                    .get_mut("herdr_core")
                    .unwrap()
                    .as_object_mut()
                    .unwrap();
                core.retain(|key, _| {
                    !matches!(
                        key.as_str(),
                        "host_start_seconds" | "host_start_microseconds" | "linux_birth"
                    )
                });
                core.insert("host_creation_time".into(), json!(1));
            },
            BIRTH,
        ),
    ]);
    #[cfg(target_os = "macos")]
    refused.push((
        "start microseconds of a whole second",
        |handle| {
            handle.get_mut("herdr_core").unwrap()["host_start_microseconds"] = json!(1_000_000);
        },
        BIRTH,
    ));
    #[cfg(target_os = "linux")]
    refused.push((
        "a boot identity of zeros",
        |handle| {
            handle.get_mut("herdr_core").unwrap()["linux_birth"]["boot_id"] =
                json!("00000000-0000-0000-0000-000000000000");
        },
        BIRTH,
    ));
    #[cfg(windows)]
    refused.extend([
        (
            "another platform's host birth",
            (|handle| {
                let core = handle
                    .get_mut("herdr_core")
                    .unwrap()
                    .as_object_mut()
                    .unwrap();
                core.remove("host_creation_time");
                core.insert("host_start_seconds".into(), json!(1_800_000_000));
                core.insert("host_start_microseconds".into(), json!(1));
            }) as Damage,
            BIRTH,
        ),
        (
            "no launch label on Windows",
            omit_label,
            "records no launch label",
        ),
    ]);
    for (label, damage, refusal) in refused {
        let fixture = Fixture::new("ready", &complete_owner());
        let store = fixture.store();
        let mut value = serde_json::to_value(store.terminal().unwrap()).unwrap();
        damage(value.as_object_mut().unwrap());
        store
            .record(CoreRecord::Terminal)
            .write_json(&value)
            .unwrap();
        fixture.remove();
        let refused = fixture
            .close(exited, false)
            .err()
            .unwrap_or_else(|| panic!("{label}: settled as absent"));
        let text = error_text(&refused);
        assert!(text.contains(refusal), "{label}: {text}");
        fixture.assert_retained(label, SessionState::Ready);
    }
}

// A legacy macOS or Linux handle has no saved label, so its terminal's
// label is checked as the generators wrote it (32 lowercase hexadecimal digits) before it is
// compared with the terminal's name. A label of the name's 24 digits, or in upper case, is not
// TAB's, and the finished terminal is left alone.
#[cfg(unix)]
#[test]
fn a_legacy_handle_accepts_only_a_launch_label_as_written() {
    for (label, launch) in [
        ("the name's 24 digits", "0123456789abcdef01234567"),
        ("upper case", "0123456789ABCDEF0123456789ABCDEF"),
    ] {
        let fixture = Fixture::new("ready", &complete_owner());
        let store = fixture.store();
        let mut value = serde_json::to_value(store.terminal().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("herdr_launch_nonce");
        store
            .record(CoreRecord::Terminal)
            .write_json(&value)
            .unwrap();
        fixture.finish();
        let mut labels = fixture.core.terminal("t7").unwrap()["labels"].clone();
        labels[LAUNCH_LABEL] = json!(launch);
        fixture.core.set("t7", "labels", labels);
        fixture.core.clear_calls();
        assert!(fixture.close(exited, false).is_err(), "{label}");
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        fixture.assert_retained(label, SessionState::Ready);
    }
}

// The closed record of a Herdr close keeps the stored diagnosis read
// under the lifecycle lock, the failed start included.
#[test]
fn a_failed_start_close_keeps_its_launch_diagnosis() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.failed_start();
    fixture.finish();
    fixture.close(exited, false).unwrap();
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(closed.error.as_deref(), Some("provider launch timed out"));
}

// A refusal's recovery command names the recorded core's endpoint.
#[test]
fn the_keep_refusal_names_the_recorded_core() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.core.set("t7", "keep", json!(true));
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains("HC_SOCKET"), "{text}");
    assert!(text.contains("keep t7 --off"), "{text}");
    // The refusal says that the recorded core answered, which
    // `doctor --probe` shows as its unverified detail.
    assert!(
        text.contains("on the recorded Herdr Core core-a, which answered at its endpoint"),
        "{text}"
    );
}

// The one granting function, row by row: what each basis and its evidence let a close send
// for the observation it is about to act on. Nothing it does not establish is granted.
#[test]
fn the_one_granting_function_grants_only_what_its_evidence_establishes() {
    use super::super::authority::{self, Basis, Grant};
    use super::super::identity::{Recorded, Requirement};
    use crate::native::terminal::close_authority::CreationEvidence;

    #[derive(Clone, Copy)]
    enum Opened {
        Owned,
        Created {
            explicit: bool,
        },
        /// A creation-record basis over a handle opened for the ordinary close.
        CreatedOverBound,
    }
    type Change = fn(&Fixture);
    let rows: [(&str, Opened, Change, bool, &str); 10] = [
        ("owned, running", Opened::Owned, |_| {}, false, "kill"),
        (
            "owned, finished",
            Opened::Owned,
            Fixture::finish,
            false,
            "remove",
        ),
        (
            "owned, running after this close's kill",
            Opened::Owned,
            |_| {},
            true,
            "wait",
        ),
        // The creation record never grants a kill (the owner's decision of 2026-11-26).
        (
            "created, running",
            Opened::Created { explicit: true },
            |_| {},
            false,
            STILL_RUNNING,
        ),
        (
            "created, running, program not observable",
            Opened::Created { explicit: true },
            Fixture::lose_program,
            false,
            STILL_RUNNING,
        ),
        (
            "created, finished",
            Opened::Created { explicit: true },
            Fixture::finish,
            false,
            "remove",
        ),
        (
            "created, another launch label",
            Opened::Created { explicit: true },
            Fixture::relabel,
            false,
            "different launch label",
        ),
        (
            "created, cleanup",
            Opened::Created { explicit: false },
            Fixture::finish,
            false,
            "only close-session --explicit",
        ),
        (
            "created over a handle opened for the ordinary close",
            Opened::CreatedOverBound,
            Fixture::finish,
            false,
            "needs a handle opened for one",
        ),
        (
            "owned, set to keep",
            Opened::Owned,
            |f| f.core.set("t7", "keep", json!(true)),
            false,
            "set to keep",
        ),
    ];
    for (label, opened, change, killed, expected) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        change(&fixture);
        let session = fixture.store().terminal().unwrap();
        let core: &dyn Core = &fixture.core;
        let requirement = match opened {
            Opened::Created { .. } => Requirement::Created(&fixture.directory),
            Opened::Owned | Opened::CreatedOverBound => Requirement::Bound(&fixture.directory),
        };
        let answer = (|| -> Result<&'static str> {
            let evidence = match opened {
                Opened::Created { explicit } => {
                    Some(authority::admit(CreationEvidence::scripted(if explicit {
                        CloseCause::Explicit
                    } else {
                        CloseCause::Cleanup
                    }))?)
                }
                Opened::CreatedOverBound => Some(authority::admit(CreationEvidence::scripted(
                    CloseCause::Explicit,
                ))?),
                Opened::Owned => None,
            };
            let (recorded, mut linked): (Recorded<'_>, _) =
                Recorded::open(core, &session, requirement, later())?.expect("present");
            let seen = linked.observe("t7", later())?.expect("present");
            let basis = match &evidence {
                Some(admitted) => Basis::Created {
                    recorded: &recorded,
                    admitted,
                },
                None => Basis::Owned(&recorded),
            };
            Ok(match authority::grant(core, &basis, &seen, killed)? {
                // A grant names the terminal it observed, on the core that answered.
                Grant::Kill(kill) => {
                    assert_eq!((kill.core_id(), kill.address()), ("core-a", "t7"));
                    "kill"
                }
                Grant::Remove(remove) => {
                    assert_eq!((remove.core_id(), remove.address()), ("core-a", "t7"));
                    "remove"
                }
                Grant::Wait => "wait",
            })
        })();
        match answer {
            Ok(answer) => assert_eq!(answer, expected, "{label}"),
            Err(error) => {
                let text = error_text(&error);
                assert!(text.contains(expected), "{label}: {text}");
            }
        }
        // Granting sends nothing by itself.
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
    }
}

// Whatever record names an owner whose
// process cannot be inspected, and while it stays uninspectable, the close never strands the
// session. Another session's or an unbound record's process is not even inspected; this
// session's own owner that cannot be inspected verifies no live owner. The running terminal is
// kept, and the refusal says why no live owner was verified for this session's own record; once
// the terminal has finished, the same command removes it; an absent one is settled.
#[test]
fn an_uninspectable_owner_process_never_strands_the_session() {
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let mut unbound = complete_owner();
    unbound.managed_session_id = None;
    for (label, owner, why) in [
        (
            "this session's record",
            complete_owner(),
            Some("its owner's process could not be inspected: owner inspection failed"),
        ),
        ("another session's record", foreign, None),
        ("an unbound record", unbound, None),
    ] {
        let fixture = Fixture::new("working", &owner);
        let text = error_text(&fixture.close(unknown, false).unwrap_err());
        assert!(text.contains(STILL_RUNNING), "{label}: {text}");
        if let Some(why) = why {
            assert!(text.contains(why), "{label}: {text}");
        }
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
        fixture.assert_retained(label, SessionState::Working);
        fixture.finish();
        fixture.core.clear_calls();
        assert_eq!(
            fixture.close(unknown, false).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Closed),
            "{label}"
        );
        assert_eq!(fixture.mutations(), (0, 1), "{label}");
        fixture.assert_closed(label);

        let fixture = Fixture::new("working", &owner);
        fixture.remove();
        assert_eq!(
            fixture.close(unknown, false).map_err(|e| error_text(&e)),
            Ok(CloseOutcome::Missing),
            "{label}, absent"
        );
        fixture.assert_closed(label);
    }
}

// The stored diagnosis survives a close interrupted after its handle
// consumption marker and before its closed record. The retry finishes from the marker without
// the adapter, and the marker carries the reason on every OS.
#[test]
fn an_interrupted_close_keeps_the_diagnosis_across_its_consumption_marker() {
    use crate::native::session::close::interruption::{self, Point};
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.failed_start();
    fixture.finish();
    let interrupted = interruption::during(Point::ConsumptionMarkerWritten, || {
        fixture.close(exited, false)
    });
    assert!(interrupted.is_err(), "the close must stop at the marker");
    let store = fixture.store();
    assert!(store.record(CoreRecord::TerminalClosed).present().unwrap());
    assert!(store.closed_if_present().unwrap().is_none());
    fixture.core.clear_calls();
    fixture.close(exited, false).unwrap();
    assert!(
        fixture.core.methods().is_empty(),
        "{:?}",
        fixture.core.methods()
    );
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(closed.error.as_deref(), Some("provider launch timed out"));
}

// A grant is minted only from an observation the verified
// connection produced, which names that connection's core. `grant` takes no core id from its
// caller and refuses an observation from another core; a grant for one core is not sent on a
// connection to another; a plain call naming either mutation is not sent. Two cores both hold
// a terminal at `t7`.
#[test]
fn a_grant_names_the_core_that_reported_its_terminal() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let session = fixture.store().terminal().unwrap();
    let core: &dyn Core = &fixture.core;
    let other = FakeCore::new();
    other.0.borrow_mut().core_id = "core-b".into();
    other
        .0
        .borrow_mut()
        .terminals
        .insert("t7".into(), fixture.core.terminal("t7").unwrap());
    let (recorded, mut linked_a) = Recorded::open(
        core,
        &session,
        Requirement::Bound(&fixture.directory),
        later(),
    )
    .unwrap()
    .expect("present");
    let socket = Path::new(&session.herdr_core().unwrap().socket);
    let mut linked_b = link(&other, socket, Some("core-b"), later()).unwrap();
    for method in ["terminal.kill", "terminal.rm"] {
        let sent = linked_a.call(method, json!({"terminal": "t7"}), later());
        assert!(matches!(sent, Err(CallError::NotSent(_))), "{method}");
    }
    // core-b's `t7` mints nothing for a handle recorded on core-a.
    let seen_b = linked_b.observe("t7", later()).unwrap().expect("present");
    let refused = authority::grant(core, &Basis::Owned(&recorded), &seen_b, false)
        .err()
        .expect("an observation from another core is refused");
    let text = error_text(&refused);
    assert!(text.contains("observed on Herdr Core core-b"), "{text}");
    // core-a's grant names core-a and is not sent to core-b.
    let seen_a = linked_a.observe("t7", later()).unwrap().expect("present");
    let Ok(Grant::Remove(remove)) =
        authority::grant(core, &Basis::Owned(&recorded), &seen_a, false)
    else {
        panic!("a finished terminal is removed");
    };
    assert_eq!((remove.core_id(), remove.address()), ("core-a", "t7"));
    assert!(
        linked_b
            .remove_confirmed(remove, later(), CloseOutcome::Missing)
            .is_err()
    );
    assert_eq!(fixture.mutations(), (0, 0));
    let sent = other.methods();
    assert!(
        !sent
            .iter()
            .any(|method| method == "terminal.rm" || method == "terminal.kill"),
        "{sent:?}"
    );
}

// A replay keeps the committed consumption marker. A close whose
// pre-close Repair failed (another session's record, its process uninspectable) and whose
// session stored a diagnosis commits both in the marker. The retry after an interruption there
// records that combined reason once, and a later explicit close of the closed session keeps the
// marker and the closed record as committed.
#[test]
fn a_replayed_consumption_marker_keeps_its_combined_reason_once() {
    use crate::native::session::close::interruption::{self, Point};
    let mut foreign = complete_owner();
    foreign.managed_session_id = Some("session-foreign".into());
    let fixture = Fixture::new("working", &foreign);
    fixture.finish();
    fixture.diagnose(SessionState::Working, "provider exited with status 7");
    let interrupted = interruption::during(Point::ConsumptionMarkerWritten, || {
        fixture.close(unknown, false)
    });
    assert!(interrupted.is_err(), "the close must stop at the marker");
    let committed = fixture
        .store()
        .terminal_closed()
        .unwrap()
        .reason
        .expect("the marker records the reason");
    assert!(
        committed.contains("pre-close session repair failed")
            && committed.contains("provider exited with status 7"),
        "{committed}"
    );
    fixture.close(unknown, false).unwrap();
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert_eq!(closed.error.as_deref(), Some(committed.as_str()));
    fixture.close(unknown, false).unwrap();
    let store = fixture.store();
    assert_eq!(
        store.terminal_closed().unwrap().reason.as_deref(),
        Some(committed.as_str())
    );
    let closed = store.closed_if_present().unwrap().unwrap();
    assert_eq!(closed.error.as_deref(), Some(committed.as_str()));
}

// The close reads the owner record once. A
// record that cannot be read (here a directory in its place) verifies no live owner, on every
// OS: a running terminal is kept, with the read failure in the refusal, and once it has
// finished the same command removes it.
#[test]
fn an_owner_record_that_cannot_be_read_never_strands_the_session() {
    let fixture = Fixture::new("working", &complete_owner());
    let owner = fixture.store().record(CoreRecord::Owner).path().to_owned();
    fs::remove_file(&owner).unwrap();
    fs::create_dir(&owner).unwrap();
    fixture.core.clear_calls();
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains(STILL_RUNNING), "{text}");
    // The running refusal states why in its own text, and only there.
    assert_eq!(
        text.matches("its owner record could not be read").count(),
        1,
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 0));
    fixture.assert_retained("an unreadable owner record", SessionState::Working);
    fixture.finish();
    fixture.core.clear_calls();
    assert_eq!(
        fixture.close(exited, false).map_err(|e| error_text(&e)),
        Ok(CloseOutcome::Closed)
    );
    assert_eq!(fixture.mutations(), (0, 1));
    fixture.assert_closed("an unreadable owner record, finished");
}

// The owner's decision of 2026-11-26: the refusal for a running terminal tells the person what
// works, in this platform's `hc` form on the recorded core: look at the terminal, end it there
// or let it finish, then close again.
#[test]
fn the_running_refusal_names_what_works() {
    let fixture = Fixture::new("ready", &complete_owner());
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    let socket = fixture
        .store()
        .terminal()
        .unwrap()
        .herdr_core()
        .unwrap()
        .socket
        .clone();
    for verb in ["get", "kill"] {
        #[cfg(unix)]
        let command = format!("HC_SOCKET='{socket}' hc {verb} t7");
        #[cfg(windows)]
        let command = format!("$env:HC_SOCKET = '{socket}'; try {{ hc {verb} t7 }}");
        assert!(text.contains(&command), "{verb}: {text}");
    }
    assert!(text.contains("run close-session again"), "{text}");
    // What TAB established, the recorded core, and the checks the
    // guide asks for before a manual kill.
    assert!(
        text.contains("TAB could not verify a live owner for this session"),
        "{text}"
    );
    assert!(!text.contains("is not live"), "{text}");
    assert!(text.contains("on the recorded Herdr Core core-a"), "{text}");
    assert!(
        text.contains(&format!("tabcli doctor {SESSION} --probe")),
        "{text}"
    );
    assert!(text.contains("this session's directory"), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
}

// The mint compares the terminal address as well as the core with
// its basis. A terminal at another address on the same core, identical in everything else, gets
// no grant.
#[test]
fn a_grant_names_the_address_its_basis_records() {
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let mut other = fixture.core.terminal("t7").unwrap();
    other["address"] = json!("t8");
    fixture
        .core
        .0
        .borrow_mut()
        .terminals
        .insert("t8".into(), other);
    let session = fixture.store().terminal().unwrap();
    let core: &dyn Core = &fixture.core;
    let (recorded, mut linked) = Recorded::open(
        core,
        &session,
        Requirement::Bound(&fixture.directory),
        later(),
    )
    .unwrap()
    .expect("present");
    let seen = linked.observe("t8", later()).unwrap().expect("present");
    let refused = authority::grant(core, &Basis::Owned(&recorded), &seen, false)
        .err()
        .expect("another address is refused");
    let text = error_text(&refused);
    assert!(text.contains("is not the terminal t7"), "{text}");
    assert_eq!(fixture.mutations(), (0, 0));
}

// A replay keeps a committed consumption marker's bytes and writes a
// damaged one again (not JSON, or `consumed: false`), as before os-parity, sending nothing to the
// core either way.
#[test]
fn a_replay_keeps_a_committed_marker_and_repairs_a_damaged_one() {
    for (label, damaged) in [
        ("not JSON", br#"{"consumed":tr"#.as_slice()),
        (
            "consumed false",
            br#"{"consumed":false,"terminal":"herdr"}"#.as_slice(),
        ),
    ] {
        let fixture = Fixture::new("ready", &complete_owner());
        fixture.finish();
        let marker = fixture
            .store()
            .record(CoreRecord::TerminalClosed)
            .path()
            .to_owned();
        fs::write(&marker, damaged).unwrap();
        fixture.core.clear_calls();
        fixture.close(exited, false).unwrap();
        assert!(
            fixture.core.methods().is_empty(),
            "{label}: {:?}",
            fixture.core.methods()
        );
        let store = fixture.store();
        assert!(store.terminal_closed().unwrap().consumed, "{label}");
        assert_eq!(
            store.status().unwrap().state,
            SessionState::Closed,
            "{label}"
        );
    }

    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let marker = fixture
        .store()
        .record(CoreRecord::TerminalClosed)
        .path()
        .to_owned();
    let committed =
        br#"{"consumed":true,"terminal":"future-terminal","reason":"kept as committed"}"#;
    fs::write(&marker, committed).unwrap();
    fixture.core.clear_calls();
    fixture.close(exited, false).unwrap();
    assert!(
        fixture.core.methods().is_empty(),
        "{:?}",
        fixture.core.methods()
    );
    assert_eq!(fs::read(&marker).unwrap(), committed);
    let closed = fixture.store().closed_if_present().unwrap().unwrap();
    assert!(
        closed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("kept as committed")),
        "{:?}",
        closed.error
    );
}

// The close with a live owner is unchanged by the owner's decision.
// Its ownership proof needs the terminal present and running on its recorded core. The surface
// part of that proof is the production one here (`verify_surface` through the Herdr adapter on
// the scripted core): a finished terminal fails it on every OS; an absent one fails it on
// Windows, and on macOS and Linux its proven absence leads to the live owner's teardown, which
// has its own checks (scripted to pass here). Nothing is sent to the core in any of these.
#[test]
fn a_live_owner_close_needs_its_terminal_running() {
    use crate::native::terminal::close_authority::{
        TerminalCloseAuthority, decide, test_support::TestObservation,
    };
    type Terminal = fn(&Fixture);
    let rows: [(&str, Terminal); 3] = [
        ("running", |_| {}),
        ("finished", Fixture::finish),
        ("absent", Fixture::remove),
    ];
    for (label, change) in rows {
        let fixture = Fixture::new("ready", &complete_owner());
        change(&fixture);
        let surface = fixture.store().terminal().unwrap();
        let deadline = later();
        fixture.core.clear_calls();
        #[cfg(unix)]
        let answer = {
            let adapters = Adapter::with_core(fixture.core.clone());
            let observation =
                TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
                    .with_evidence(crate::native::terminal::close_evidence(TerminalKind::Herdr))
                    .with_posix_owner_live(|_| live_other())
                    .with_proof(|| {
                        crate::native::terminal::posix::verify_surface(
                            &adapters,
                            &fixture.directory,
                            &surface,
                            None,
                        )
                        .map(|_| ())
                    })
                    .with_presence(|| {
                        crate::native::terminal::posix::surface_present(
                            &adapters,
                            &fixture.directory,
                            &surface,
                            std::time::Duration::from_secs(1),
                        )
                    })
                    .with_attest(|| Ok(()));
            decide(&observation, SESSION, &surface, deadline)
        };
        #[cfg(windows)]
        let answer = {
            let dispatch = crate::native::terminal::windows_dispatch::Dispatch::new(
                Adapter::with_core(fixture.console.clone()),
                Adapter::with_core(fixture.core.clone()),
            );
            let observation =
                TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
                    .with_owner_record(|_| live_other())
                    .with_proof(|| {
                        crate::native::terminal::windows_dispatch::verify_surface(
                            &dispatch, &surface, None,
                        )
                    });
            decide(&observation, SESSION, &surface, deadline)
        };
        match label {
            "running" => assert_eq!(answer.unwrap(), TerminalCloseAuthority::LiveOwner),
            "finished" => {
                let text = error_text(&answer.unwrap_err());
                assert!(text.contains("has finished"), "{label}: {text}");
            }
            _ => {
                #[cfg(unix)]
                assert_eq!(answer.unwrap(), TerminalCloseAuthority::OwnerTeardown);
                #[cfg(windows)]
                assert!(answer.is_err(), "{label}: {answer:?}");
            }
        }
        assert_eq!(fixture.mutations(), (0, 0), "{label}");
    }
}

// The attestation reads the owner's birth, then its control identity.
// When the birth matched but the control read finds no process at the PID, the owner exited
// during attestation: the decision takes the creation record (with the diagnosis), instead of
// choosing the close with a live owner and refusing there. macOS and Linux only.
#[cfg(unix)]
#[test]
fn an_owner_that_exits_between_its_two_reads_takes_the_creation_record() {
    use crate::native::session::owner::{ControlAttestation, OwnerObservers, attest_owner_with};
    use crate::native::terminal::close_authority::{
        TerminalCloseAuthority, decide, test_support::TestObservation,
    };
    use std::cell::RefCell;
    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let surface = fixture.store().terminal().unwrap();
    let deadline = later();
    let reads = RefCell::new(Vec::new());
    let attest = |owner: &NativeSessionOwner| {
        attest_owner_with(
            owner,
            OwnerObservers {
                pid_alive: &|_| true,
                #[cfg(target_os = "macos")]
                birth: &|_| {
                    reads.borrow_mut().push("birth");
                    Ok(owner
                        .process_start_seconds
                        .zip(owner.process_start_microseconds))
                },
                #[cfg(target_os = "linux")]
                birth: &|_| {
                    reads.borrow_mut().push("birth");
                    Ok(owner
                        .linux_birth
                        .map(crate::native::terminal::ownership::ProcessBirth::Linux))
                },
                control: &|_| {
                    reads.borrow_mut().push("control");
                    Ok(None)
                },
            },
        )
    };
    let attestation = attest(&complete_owner());
    assert!(matches!(attestation.control, ControlAttestation::Exited));
    reads.borrow_mut().clear();
    let observation = TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
        .with_evidence(crate::native::terminal::close_evidence(TerminalKind::Herdr))
        .with_posix_owner_live(attest)
        .with_proof(|| panic!("the creation record asks no ownership proof"))
        .with_presence(|| panic!("the creation record asks no presence"));
    let answer = decide(&observation, SESSION, &surface, deadline);
    assert_eq!(*reads.borrow(), ["birth", "control"]);
    let Ok(TerminalCloseAuthority::CreatedSurface(evidence)) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(
        evidence.generated_reason().as_deref(),
        Some("native session process 4242 is no longer running")
    );
    assert_eq!(fixture.mutations(), (0, 0));
}

#[cfg(unix)]
fn save_teardown(fixture: &Fixture, surface: &TerminalSession, owner: &NativeSessionOwner) {
    session::close::record_terminal_teardown_intent(
        &fixture.store(),
        SESSION,
        surface,
        owner,
        owner.pid,
        "owner teardown",
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn saved_teardown_with_a_contradictory_owner_sends_no_group_signal() {
    use crate::native::terminal::close_authority::{
        close_owned_surface_with, decide, test_support::TestObservation,
    };
    use crate::native::terminal::posix;
    use std::cell::Cell;
    let mut owner = complete_owner();
    owner.windows_process_identity = Some(crate::native::terminal::WindowsProcessIdentity {
        creation_time: 42,
        executable_path: "tabcli.exe".into(),
    });
    let fixture = Fixture::new("ready", &owner);
    let surface = fixture.store().terminal().unwrap();
    save_teardown(&fixture, &surface, &owner);
    fixture.remove();
    let deadline = later();
    let adapters = Adapter::with_core(fixture.core.clone());
    let signals = Cell::new(0);
    let observation = TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
        .with_posix_owner_live(|_| live_other())
        .with_proof(|| Err(anyhow!("surface absent")))
        .with_presence(|| {
            posix::surface_present(
                &adapters,
                &fixture.directory,
                &surface,
                Duration::from_secs(1),
            )
        })
        .with_attest(|| Ok(()));
    let result = close_owned_surface_with(
        || decide(&observation, SESSION, &surface, deadline),
        || {
            signals.set(signals.get() + 1);
            Ok(CloseOutcome::Missing.into())
        },
        |close| {
            posix::close_owned_session(
                &adapters,
                &fixture.directory,
                SESSION,
                &surface,
                close,
                deadline,
            )
        },
        deadline,
    )
    .unwrap();
    assert_eq!(signals.get(), 0);
    assert_eq!(result.outcome, CloseOutcome::Missing);
    assert!(
        fixture
            .core
            .methods()
            .iter()
            .any(|method| method == "terminal.get")
    );
}

#[cfg(unix)]
#[test]
fn saved_teardown_with_a_replacement_owner_settles_an_absent_terminal() {
    for replacement in [None, Some("session-foreign".into())] {
        let mut owner = complete_owner();
        let fixture = Fixture::new("ready", &owner);
        save_teardown(&fixture, &fixture.store().terminal().unwrap(), &owner);
        fixture.claim();
        owner.managed_session_id = replacement;
        fixture.store().write_owner(&owner).unwrap();
        fixture.remove();
        assert_eq!(fixture.close(exited, false).unwrap(), CloseOutcome::Missing);
        fixture.assert_closed("replacement owner");
    }
}

#[cfg(unix)]
#[test]
fn saved_teardown_with_the_wrong_handle_and_no_owner_keeps_the_handle() {
    let owner = complete_owner();
    let fixture = Fixture::new("ready", &owner);
    let mut wrong = fixture.store().terminal().unwrap();
    wrong.id = "t8".into();
    save_teardown(&fixture, &wrong, &owner);
    fs::remove_file(fixture.store().record(CoreRecord::Owner).path()).unwrap();
    fixture.remove();
    assert!(fixture.close(exited, false).is_err());
    fixture.assert_retained("wrong handle", SessionState::Ready);
    assert!(fixture.core.methods().is_empty());
}

#[cfg(unix)]
#[test]
fn saved_teardown_reads_the_owner_once_when_the_first_read_fails() {
    use crate::native::terminal::close_authority::{
        OwnerRead, TerminalCloseAuthority, decide, test_support::TestObservation,
    };
    use std::cell::Cell;
    let owner = complete_owner();
    let fixture = Fixture::new("ready", &owner);
    let surface = fixture.store().terminal().unwrap();
    save_teardown(&fixture, &surface, &owner);
    fs::remove_file(fixture.store().record(CoreRecord::Owner).path()).unwrap();
    let reads = Cell::new(0);
    let deadline = later();
    let observation = TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
        .with_read_owner(|| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                anyhow::bail!("first owner read failed");
            }
            Ok(OwnerRead::Recorded(Box::new(owner.clone())))
        })
        .with_posix_owner_live(|_| live_other());
    let result = decide(&observation, SESSION, &surface, deadline);
    assert_eq!(reads.get(), 1);
    assert!(
        matches!(result, Ok(TerminalCloseAuthority::CreatedSurface(evidence))
        if evidence.unverified().unwrap_or_default().contains("first owner read failed"))
    );
}

#[cfg(unix)]
#[test]
fn saved_teardown_refuses_a_different_attested_live_owner() {
    use crate::native::terminal::close_authority::{decide, test_support::TestObservation};
    let mut owner = complete_owner();
    let fixture = Fixture::new("ready", &owner);
    let surface = fixture.store().terminal().unwrap();
    save_teardown(&fixture, &surface, &owner);
    owner.terminal_tty_device = Some(43);
    fixture.store().write_owner(&owner).unwrap();
    let deadline = later();
    let observation = TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
        .with_posix_owner_live(|_| live_other())
        .with_proof(|| panic!("a different live owner cannot replay the intent"));
    let error = decide(&observation, SESSION, &surface, deadline).unwrap_err();
    assert!(error_text(&error).contains("does not match the attested owner"));
}

#[cfg(unix)]
#[test]
fn ghostty_saved_teardown_still_requires_the_recorded_owner() {
    let owner = complete_owner();
    let fixture = Fixture::new("failed", &owner);
    let surface = TerminalSession::new(TerminalKind::Ghostty, "fixture".into())
        .with_binding(Some(SESSION.into()));
    save_teardown(&fixture, &surface, &owner);
    fs::remove_file(fixture.store().record(CoreRecord::Owner).path()).unwrap();
    let deadline = later();
    let observation =
        crate::native::terminal::close_authority::test_support::TestObservation::on_records(
            &fixture.directory,
            SESSION,
            &surface,
            deadline,
        );
    assert!(observation.decide(SESSION, &surface, deadline).is_err());
    assert_eq!(observation.calls(), ["teardown-intent"]);
}

#[cfg(unix)]
#[test]
fn saved_teardown_without_a_readable_owner_uses_the_creation_record() {
    for unreadable in [false, true] {
        for state in ["absent", "finished", "running"] {
            let owner = complete_owner();
            let fixture = Fixture::new("ready", &owner);
            save_teardown(&fixture, &fixture.store().terminal().unwrap(), &owner);
            let path = fixture.store().record(CoreRecord::Owner).path().to_owned();
            fs::remove_file(&path).unwrap();
            if unreadable {
                fs::create_dir(path).unwrap();
            }
            match state {
                "absent" => fixture.remove(),
                "finished" => fixture.finish(),
                _ => {}
            }
            let result = fixture.close(exited, false);
            if state == "running" {
                assert!(error_text(&result.unwrap_err()).contains(STILL_RUNNING));
                fixture.assert_retained(state, SessionState::Ready);
            } else {
                assert_eq!(
                    result.unwrap(),
                    if state == "finished" {
                        CloseOutcome::Closed
                    } else {
                        CloseOutcome::Missing
                    }
                );
                fixture.assert_closed(state);
            }
            assert_eq!(fixture.mutations(), (0, usize::from(state == "finished")));
        }
    }
}

// The close with a live owner needs its full ownership proof. A bound
// record whose birth matches but which lacks the TTY and process-group fields is refused by the
// production proof (the Herdr adapter's surface check, then the owner check, here with the
// process reads scripted), and nothing is sent. macOS and Linux only.
#[cfg(unix)]
#[test]
fn a_live_owner_without_its_control_record_is_refused_by_the_full_proof() {
    use crate::native::terminal::close_authority::{decide, test_support::TestObservation};
    use crate::native::terminal::ownership::{NativeProcessIdentity, ProcessBirth};
    use crate::native::terminal::posix_ownership::{
        ProcessReads, verified_posix_terminal_owner_with,
    };
    let mut owner = complete_owner();
    owner.terminal_tty = None;
    owner.terminal_tty_device = None;
    owner.process_group = None;
    owner.terminal_process_group = None;
    let fixture = Fixture::new("ready", &owner);
    let surface = fixture.store().terminal().unwrap();
    let deadline = later();
    #[cfg(target_os = "macos")]
    let birth = ProcessBirth::Macos(1_790_000_000, 42);
    #[cfg(target_os = "linux")]
    let birth = ProcessBirth::Linux(complete_owner().linux_birth.unwrap());
    let identity = move |pid: u32| {
        Ok(NativeProcessIdentity {
            pid,
            parent_pid: 1,
            terminal_tty_device: 42,
            process_group: 4242,
            terminal_process_group: 4242,
            birth,
        })
    };
    let tty_device = |_: &std::path::Path| Ok(42);
    let reads = ProcessReads {
        identity: &identity,
        tty_device: &tty_device,
    };
    let adapters = Adapter::with_core(fixture.core.clone());
    let observation = TestObservation::on_records(&fixture.directory, SESSION, &surface, deadline)
        .with_evidence(crate::native::terminal::close_evidence(TerminalKind::Herdr))
        .with_posix_owner_live(|_| live_other())
        .with_proof(|| {
            let tty = crate::native::terminal::posix::verify_surface(
                &adapters,
                &fixture.directory,
                &surface,
                None,
            )?;
            verified_posix_terminal_owner_with(
                &fixture.directory,
                SESSION,
                &surface,
                tty.as_deref(),
                reads,
            )
            .map(|_| ())
        })
        .with_presence(|| {
            crate::native::terminal::posix::surface_present(
                &adapters,
                &fixture.directory,
                &surface,
                std::time::Duration::from_secs(1),
            )
        })
        .with_attest(|| panic!("a present terminal is not torn down"));
    let text = error_text(&decide(&observation, SESSION, &surface, deadline).unwrap_err());
    assert!(
        text.contains("birth, TTY, or foreground identity changed"),
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 0));
}

// A refusal on the creation record keeps why no live owner was
// verified, beside its own recovery instructions (here a running terminal set to keep, with an
// owner record that cannot be read); the cleanup after a refused reopen says that TAB could not
// verify a live owner.
#[test]
fn a_refusal_keeps_why_no_live_owner_was_verified() {
    let fixture = Fixture::new("working", &complete_owner());
    fixture.core.set("t7", "keep", json!(true));
    let owner = fixture.store().record(CoreRecord::Owner).path().to_owned();
    fs::remove_file(&owner).unwrap();
    fs::create_dir(&owner).unwrap();
    let text = error_text(&fixture.close(exited, false).unwrap_err());
    assert!(text.contains("set to keep"), "{text}");
    assert!(text.contains("keep t7 --off"), "{text}");
    assert!(
        text.contains("TAB could not verify a live owner for this session"),
        "{text}"
    );
    assert_eq!(
        text.matches("its owner record could not be read").count(),
        1,
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 0));

    let fixture = Fixture::new("ready", &complete_owner());
    fixture.finish();
    let text = error_text(
        &fixture
            .close_as(unknown, false, CloseCause::Cleanup)
            .unwrap_err(),
    );
    assert!(
        text.contains("TAB could not verify a live owner for this session"),
        "{text}"
    );
    assert!(text.contains("only close-session --explicit"), "{text}");
    assert!(
        !text.contains("no live owner of this session holds"),
        "{text}"
    );
    assert_eq!(fixture.mutations(), (0, 0));
}
