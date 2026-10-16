// The characterization of the decision: every row names its Records and observations,
// the authority or error it expects, and the ordered sequence of seam calls, Record
// reads included. Rows with a close intent are Record-backed: the intent is written
// by the production writer and read by the production helper, so the test adapter
// forwards the Record accessors to the real Reader and injects only the observations.
use super::test_support::{Call, RecordValues, TestObservation};
use super::*;
use crate::native::session::OwnerObservation;
use crate::native::session::owner::ControlAttestation;
use crate::native::terminal::ownership::{MacTerminalAppIdentity, test_owner_attestation};
use std::cell::RefCell;
use std::collections::HashMap;
use terminal::TerminalKind::*;

const ID: &str = "session-close-table";
const DEAD_IDENTITY: &str = "identity is incomplete, foreign, or its PID is reused";
const DEAD_PRESENT: &str = "visible terminal cleanup is unverified";

// The Record reads of a row with an owner record before its first observation: the
// teardown intent, the owner's presence by text, and the launch record.
const OWNED: [&str; 3] = ["teardown-intent", "owner-present", "launch"];
// A failed start that may qualify for the stable-id shortcut reads status, then the
// owner, then observes that owner, after the launch record.
const FAILED_START: [&str; 6] = [
    "teardown-intent",
    "owner-present",
    "launch",
    "status",
    "owner",
    "owner-record",
];
// A row without an owner record reads status once, in the no-owner branch.
const OWNERLESS: [&str; 3] = ["teardown-intent", "owner-present", "status"];

fn with(prefix: &[&'static str], rest: &[&'static str]) -> Vec<&'static str> {
    prefix.iter().chain(rest).copied().collect()
}

fn app() -> MacTerminalAppIdentity {
    MacTerminalAppIdentity {
        pid: 456,
        start_seconds: 100,
        start_microseconds: 42,
    }
}

fn handle(kind: terminal::TerminalKind, surface_binding: Option<String>) -> TerminalSession {
    TerminalSession {
        kind,
        id: "surface-id".into(),
        tab_id: None,
        window_id: Some("1001".into()),
        managed_session_id: surface_binding,
        windows_process_identity: None,
        wezterm_mux: None,
    }
}

fn snapshot(directory: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut records: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_owned(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect();
    records.sort();
    records
}

struct Row {
    kind: terminal::TerminalKind,
    owner: Option<NativeSessionOwner>,
    state: SessionState,
    phase: Option<launch::Phase>,
    intent: bool,
    // Record-backed without an intent: the Records are written and read for real.
    backed: bool,
    surface_binding: Option<String>,
    observation_error: Option<(&'static str, usize)>,
    observed_alive: Option<bool>,
    observed_matches: Option<bool>,
    pid_alive: bool,
    live: bool,
    proof_error: bool,
    present: bool,
    birth: Option<(u64, u64)>,
    instances: Vec<MacTerminalAppIdentity>,
}

impl Row {
    fn new(kind: terminal::TerminalKind) -> Self {
        Self {
            kind,
            owner: Some(NativeSessionOwner {
                pid: 123,
                managed_session_id: Some(ID.into()),
                terminal_tty: Some("/dev/ttys999".into()),
                terminal_tty_device: Some(7),
                process_start_seconds: Some(90),
                process_start_microseconds: Some(42),
                process_group: Some(123),
                terminal_process_group: Some(123),
                terminal_app: (kind == AppleTerminal).then(app),
                ..NativeSessionOwner::default()
            }),
            state: SessionState::Ready,
            phase: None,
            intent: false,
            backed: false,
            surface_binding: Some(ID.into()),
            observation_error: None,
            observed_alive: Some(false),
            observed_matches: None,
            pid_alive: false,
            live: false,
            proof_error: false,
            present: false,
            birth: Some((100, 42)),
            instances: vec![app()],
        }
    }

    fn launch_record(&self) -> Option<launch::Record> {
        self.phase.map(|phase| launch::Record {
            schema: 1,
            claim_token: "claim-table".into(),
            deadline_unix_ms: 1,
            phase,
        })
    }

    // Writes the row's Records as production writes them.
    fn write(&self, directory: &Path, handle: &TerminalSession) {
        let store = Store::open_unchecked(directory);
        store
            .record(CoreRecord::Terminal)
            .write_json(handle)
            .unwrap();
        store
            .record(CoreRecord::Status)
            .write_json(&SessionStatus {
                state: self.state.clone(),
                generation: 0,
                updated_unix_ms: 1,
                exit_code: None,
                error: None,
                residual_surface: None,
            })
            .unwrap();
        if let Some(owner) = &self.owner {
            store.record(CoreRecord::Owner).write_json(owner).unwrap();
            if self.intent {
                session::close::record_terminal_close_intent(&store, ID, handle, owner).unwrap();
            }
        }
        if let Some(launch) = self.launch_record() {
            store
                .record(CoreRecord::Launch)
                .write_json(&launch)
                .unwrap();
        }
    }

    fn check(
        self,
        expected: std::result::Result<TerminalCloseAuthority, &str>,
        calls: &[&str],
    ) -> Vec<Call> {
        self.check_with_error_chain(expected, Some(calls), None)
    }

    fn check_error_chain(self, chain: &[&str], calls: &[&str]) -> Vec<Call> {
        self.check_with_error_chain(Err(""), Some(calls), Some(chain))
    }

    // Decides the row, asserts the ordered seam calls and the expected outcome, and for
    // a Record-backed row asserts that the production reader changed no record (#101).
    fn check_with_error_chain(
        self,
        expected: std::result::Result<TerminalCloseAuthority, &str>,
        calls: Option<&[&str]>,
        error_chain: Option<&[&str]>,
    ) -> Vec<Call> {
        let handle = handle(self.kind, self.surface_binding.clone());
        let directory = (self.intent || self.backed).then(|| tempfile::tempdir().unwrap());
        if let Some(directory) = &directory {
            self.write(directory.path(), &handle);
        }
        let before = directory
            .as_ref()
            .map(|directory| snapshot(directory.path()));
        let trace = RefCell::new(Vec::new());
        let counts = RefCell::new(HashMap::new());
        let note = |call: &'static str| trace.borrow_mut().push(Call::new(call, ""));
        let fault = |call: &'static str| -> Result<()> {
            let mut counts = counts.borrow_mut();
            let occurrence = counts.entry(call).or_insert(0);
            *occurrence += 1;
            if self.observation_error == Some((call, *occurrence)) {
                return Err(anyhow::anyhow!("injected {call} cause"))
                    .with_context(|| format!("injected {call} failure"));
            }
            Ok(())
        };
        let apple = terminal::macos::apple_terminal::close_evidence::with_observations(
            |pid| {
                assert_eq!(pid, app().pid);
                note("birth");
                fault("birth")?;
                Ok(self.birth)
            },
            || {
                note("instances");
                fault("instances")?;
                Ok(self.instances.clone())
            },
        );
        let deadline = crate::native::session::close::budget::close_deadline();
        let observation = match &directory {
            Some(directory) => TestObservation::on_records(directory.path(), ID, &handle, deadline),
            None => TestObservation::from_values(RecordValues {
                owner: self.owner.clone(),
                state: self.state.clone(),
                launch: self.launch_record(),
            }),
        };
        let observation = observation
            .with_trace(&trace)
            .with_presence(|| {
                fault("presence")?;
                Ok(self.present)
            })
            .with_evidence(if handle.kind == terminal::TerminalKind::AppleTerminal {
                &apple
            } else {
                terminal::close_evidence(handle.kind)
            })
            .with_owner_record(|owner| {
                assert_eq!(owner.pid, 123);
                OwnerObservation {
                    process_alive: self.observed_alive,
                    identity_matches: self.observed_matches,
                    error: None,
                }
            })
            .with_pid_alive(|pid| {
                assert_eq!(pid, 123);
                self.pid_alive
            })
            .with_mac_owner_live(|owner| {
                assert_eq!(owner.pid, 123);
                match fault("mac-live") {
                    Ok(()) => test_owner_attestation(self.live),
                    Err(error) => OwnerAttestation {
                        process: ProcessObservation::Unknown(format!("{error:#}")),
                        control: ControlAttestation::NotRecorded,
                        pid_alive: Some(self.pid_alive),
                    },
                }
            })
            // Read the real intent and owner binding, substituting only its final
            // process observation. Do not let a test boolean invent an intent.
            .with_resumable(|| {
                fault("resumable")?;
                let Some(directory) = &directory else {
                    return Ok(false);
                };
                Ok(session::close::terminal_close_intent_owner(
                    &Reader::open_unchecked(directory.path()),
                    &handle,
                    terminal::surface_outlives_owner(&handle),
                )?
                .is_some_and(|owner| {
                    assert_eq!(owner.pid, 123);
                    note("resume-pid-alive");
                    !self.pid_alive
                }))
            })
            .with_attest(|| fault("attest"))
            .with_group_gone(|_| panic!("ordinary table row has no teardown intent"))
            .with_proof(|| {
                if self.proof_error {
                    bail!("injected full ownership proof failure")
                }
                Ok(())
            });
        let result = observation.decide(ID, &handle, deadline);
        if let Some(calls) = calls {
            assert_eq!(observation.calls(), calls, "{:?}: {result:?}", self.kind);
        }
        if let Some(chain) = error_chain {
            let error = result.as_ref().unwrap_err();
            assert_eq!(
                error.chain().map(ToString::to_string).collect::<Vec<_>>(),
                chain,
                "{:?}: error chain changed",
                self.kind
            );
        }
        match expected {
            Ok(authority) => assert_eq!(result.unwrap(), authority, "{:?}", self.kind),
            Err(message) => assert!(
                format!("{:#}", result.unwrap_err()).contains(message),
                "{:?}: expected {message}",
                self.kind
            ),
        }
        if let (Some(directory), Some(before)) = (&directory, before) {
            assert_eq!(
                snapshot(directory.path()),
                before,
                "the decision changed session records"
            );
        }
        observation.trace()
    }
}

#[test]
fn r3_live_owner_absent_surface_requires_teardown() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        let mut row = Row::new(kind);
        row.live = true;
        row.proof_error = true;
        let mut calls = with(&OWNED, &["resumable", "owner", "mac-live"]);
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("proof");
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("presence");
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("attest");
        row.check(Ok(TerminalCloseAuthority::OwnerTeardown), &calls);
    }
}

#[test]
fn r3_ended_terminal_incarnation_requires_teardown() {
    let mut row = Row::new(AppleTerminal);
    row.live = true;
    row.birth = None;
    row.check(
        Ok(TerminalCloseAuthority::OwnerTeardown),
        &with(
            &OWNED,
            &["resumable", "owner", "mac-live", "birth", "birth", "attest"],
        ),
    );
}

#[test]
fn r3_presence_present_unknown_or_error_preserves_original_proof_error() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        for presence in ["present", "unknown", "error"] {
            let mut row = Row::new(kind);
            row.live = true;
            row.proof_error = true;
            row.present = true;
            if presence != "present" {
                row.observation_error = Some(("presence", 1));
            }
            let mut calls = with(&OWNED, &["resumable", "owner", "mac-live"]);
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("proof");
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("presence");
            if kind == AppleTerminal && presence == "present" {
                calls.extend(["birth", "instances"]);
            }
            row.check_error_chain(&["injected full ownership proof failure"], &calls);
        }
    }
}

#[test]
fn r3_absence_then_failed_attestation_writes_nothing() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        let mut row = Row::new(kind);
        row.live = true;
        row.proof_error = true;
        row.observation_error = Some(("attest", 1));
        let mut calls = with(&OWNED, &["resumable", "owner", "mac-live"]);
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("proof");
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("presence");
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("attest");
        row.check_error_chain(
            &["injected attest failure", "injected attest cause"],
            &calls,
        );
    }
}

#[test]
fn r1_c6_reused_projection_qualifies_only_failed_bound_startup_surface() {
    let observed = crate::native::session::owner::tests::reused_observation();
    for kind in [Iterm2, Ghostty] {
        let mut row = Row::new(kind);
        row.state = SessionState::Failed;
        row.phase = Some(launch::Phase::Spawning);
        row.observed_alive = observed.process_alive;
        row.observed_matches = observed.identity_matches;
        row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &FAILED_START);
    }
}

#[test]
fn failed_start_native_identity_dead_or_mismatched_owner_surface_only() {
    for kind in [Iterm2, Ghostty] {
        for (alive, matches) in [(Some(false), None), (Some(true), Some(false))] {
            let mut row = Row::new(kind);
            row.state = SessionState::Failed;
            row.phase = Some(launch::Phase::Spawning);
            row.observed_alive = alive;
            row.observed_matches = matches;
            row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &FAILED_START);
        }
    }
}

#[test]
fn failed_start_live_matching_or_unknown_owner_gains_no_shortcut() {
    for kind in [Iterm2, Ghostty] {
        for matches in [Some(true), None] {
            let mut row = Row::new(kind);
            row.state = SessionState::Failed;
            row.phase = Some(launch::Phase::Pending);
            row.observed_alive = Some(true);
            row.observed_matches = matches;
            row.live = true;
            row.proof_error = true;
            row.present = true;
            row.check(
                Err("injected full ownership proof failure"),
                &with(
                    &FAILED_START,
                    &["resumable", "owner", "mac-live", "proof", "presence"],
                ),
            );
        }
    }
}

#[test]
fn failed_start_shortcut_requires_failed_bound_owner_and_unspawned_launch() {
    for case in ["ready", "foreign", "spawned"] {
        let mut row = Row::new(Iterm2);
        row.phase = Some(launch::Phase::Spawning);
        row.state = SessionState::Failed;
        match case {
            "ready" => row.state = SessionState::Ready,
            "foreign" => row.owner.as_mut().unwrap().managed_session_id = Some("foreign".into()),
            _ => row.phase = Some(launch::Phase::Spawned),
        }
        // A spawned launch reads the launch record and nothing more of the failed start.
        let mut calls = if case != "spawned" {
            FAILED_START.to_vec()
        } else {
            OWNED.to_vec()
        };
        calls.extend(["resumable", "owner", "mac-live", "pid-alive"]);
        if case == "foreign" {
            row.check(Err(DEAD_IDENTITY), &calls);
        } else {
            calls.push("presence");
            row.check(Ok(TerminalCloseAuthority::Absent), &calls);
        }
    }
}

#[test]
fn resumable_close_intent_native_surfaces_surface_only() {
    for kind in [Warp, WezTerm, Ghostty] {
        let mut row = Row::new(kind);
        row.intent = true;
        row.check(
            Ok(TerminalCloseAuthority::SurfaceOnly),
            &with(&OWNED, &["resumable", "resume-pid-alive"]),
        );
    }
}

#[test]
fn close_intent_live_pid_does_not_resume() {
    let mut row = Row::new(Ghostty);
    row.intent = true;
    row.pid_alive = true;
    row.live = true;
    row.check(
        Ok(TerminalCloseAuthority::LiveOwner),
        &with(
            &OWNED,
            &[
                "resumable",
                "resume-pid-alive",
                "owner",
                "mac-live",
                "proof",
            ],
        ),
    );
}

// Terminal.app's resumable evidence reads the owner itself, after the intent matched.
#[test]
fn resumable_terminal_recorded_incarnation_surface_only_absent_or_error() {
    for case in ["live", "ended", "reused", "ambiguous"] {
        let mut row = Row::new(AppleTerminal);
        row.intent = true;
        let mut calls = with(&OWNED, &["resumable", "resume-pid-alive", "owner", "birth"]);
        let expected = match case {
            "ended" => {
                row.birth = None;
                Ok(TerminalCloseAuthority::Absent)
            }
            "reused" => {
                row.birth = Some((101, 42));
                Err("PID was reused")
            }
            "ambiguous" => {
                row.instances.clear();
                calls.push("instances");
                Err("scripting target is ambiguous or changed")
            }
            _ => {
                calls.push("instances");
                Ok(TerminalCloseAuthority::SurfaceOnly)
            }
        };
        row.check(expected, &calls);
    }
}

#[test]
fn resumable_terminal_without_incarnation_absent_or_error() {
    for present in [false, true] {
        let mut row = Row::new(AppleTerminal);
        row.intent = true;
        row.owner.as_mut().unwrap().terminal_app = None;
        row.present = present;
        row.check(
            if present {
                Err("process incarnation was not recorded")
            } else {
                Ok(TerminalCloseAuthority::Absent)
            },
            &with(
                &OWNED,
                &[
                    "resumable",
                    "resume-pid-alive",
                    "owner",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
        );
    }
}

#[test]
fn dead_owner_incomplete_foreign_or_reused_pid_refused_before_surface() {
    for case in [
        "incomplete",
        "foreign",
        "reused",
        "missing-tty",
        "empty-tty",
    ] {
        let mut row = Row::new(Iterm2);
        let owner = row.owner.as_mut().unwrap();
        match case {
            "incomplete" => owner.process_group = None,
            "foreign" => owner.managed_session_id = Some("foreign".into()),
            "reused" => row.pid_alive = true,
            "missing-tty" => owner.terminal_tty = None,
            _ => owner.terminal_tty = Some(String::new()),
        }
        row.check(
            Err(DEAD_IDENTITY),
            &with(&OWNED, &["resumable", "owner", "mac-live", "pid-alive"]),
        );
    }
}

#[test]
fn dead_owner_surface_absent_or_present() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        for present in [false, true] {
            let mut row = Row::new(kind);
            row.present = present;
            let mut calls = with(&OWNED, &["resumable", "owner", "mac-live", "pid-alive"]);
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("presence");
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            row.check(
                if present {
                    Err(DEAD_PRESENT)
                } else {
                    Ok(TerminalCloseAuthority::Absent)
                },
                &calls,
            );
        }
    }
}

#[test]
fn dead_terminal_owner_ended_incarnation_absent_without_surface_query() {
    let mut row = Row::new(AppleTerminal);
    row.birth = None;
    row.check(
        Ok(TerminalCloseAuthority::Absent),
        &with(
            &OWNED,
            &["resumable", "owner", "mac-live", "pid-alive", "birth"],
        ),
    );
}

#[test]
fn live_owner_full_proof_success_or_original_error() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        for proof_error in [false, true] {
            let mut row = Row::new(kind);
            row.live = true;
            row.proof_error = proof_error;
            row.present = true;
            let mut calls = with(&OWNED, &["resumable", "owner", "mac-live"]);
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("proof");
            if proof_error {
                if kind == AppleTerminal {
                    calls.extend(["birth", "instances"]);
                }
                calls.push("presence");
                if kind == AppleTerminal {
                    calls.extend(["birth", "instances"]);
                }
            }
            row.check(
                if proof_error {
                    Err("injected full ownership proof failure")
                } else {
                    Ok(TerminalCloseAuthority::LiveOwner)
                },
                &calls,
            );
        }
    }
}

#[test]
fn live_terminal_owner_ended_incarnation_teardown() {
    let mut row = Row::new(AppleTerminal);
    row.live = true;
    row.birth = None;
    row.check(
        Ok(TerminalCloseAuthority::OwnerTeardown),
        &with(
            &OWNED,
            &["resumable", "owner", "mac-live", "birth", "birth", "attest"],
        ),
    );
}

#[test]
fn ownerless_non_startup_status_refused_without_observation() {
    for state in [
        SessionState::Ready,
        SessionState::Working,
        SessionState::Closed,
    ] {
        let mut row = Row::new(Iterm2);
        row.owner = None;
        row.state = state;
        row.check(
            Err("terminal close requires a live native-session owner"),
            &OWNERLESS,
        );
    }
}

#[test]
fn ownerless_terminal_startup_absent_or_present() {
    for state in [SessionState::Launching, SessionState::Failed] {
        for present in [false, true] {
            let mut row = Row::new(AppleTerminal);
            row.owner = None;
            row.state = state.clone();
            row.present = present;
            row.check(
                if present {
                    Err("startup has no native owner/app incarnation")
                } else {
                    Ok(TerminalCloseAuthority::Absent)
                },
                &with(&OWNERLESS, &["instances", "presence", "instances"]),
            );
        }
    }
}

#[test]
fn ownerless_warp_wezterm_startup_absent_or_present() {
    for kind in [Warp, WezTerm] {
        for state in [SessionState::Launching, SessionState::Failed] {
            for present in [false, true] {
                let mut row = Row::new(kind);
                row.owner = None;
                row.state = state.clone();
                row.present = present;
                row.check(
                    if present {
                        Err("startup has no native owner or close intent")
                    } else {
                        Ok(TerminalCloseAuthority::Absent)
                    },
                    &with(&OWNERLESS, &["presence"]),
                );
            }
        }
    }
}

#[test]
fn ownerless_native_close_identity_startup_surface_only() {
    for kind in [Iterm2, Ghostty] {
        for state in [SessionState::Launching, SessionState::Failed] {
            let mut row = Row::new(kind);
            row.owner = None;
            row.state = state;
            row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &OWNERLESS);
        }
    }
}

#[test]
fn resumability_error_stops_before_owner_liveness() {
    let mut row = Row::new(Ghostty);
    row.intent = true;
    row.observation_error = Some(("resumable", 1));
    row.check_error_chain(
        &["injected resumable failure", "injected resumable cause"],
        &with(&OWNED, &["resumable"]),
    );
}

#[test]
fn mac_owner_liveness_error_stops_before_pid_surface_and_proof() {
    let mut row = Row::new(AppleTerminal);
    row.observation_error = Some(("mac-live", 1));
    row.check_error_chain(
        &["injected mac-live failure: injected mac-live cause"],
        &with(&OWNED, &["resumable", "owner", "mac-live"]),
    );
}

#[test]
fn dead_owner_presence_error_never_grants_absence() {
    for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
        let mut row = Row::new(kind);
        row.observation_error = Some(("presence", 1));
        let mut calls = with(&OWNED, &["resumable", "owner", "mac-live", "pid-alive"]);
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("presence");
        row.check_error_chain(
            &[
                "could not prove absence of the dead owner's exact terminal surface; no close was sent",
                "injected presence failure",
                "injected presence cause",
            ],
            &calls,
        );
    }
}

#[test]
fn ownerless_startup_presence_error_never_grants_absence() {
    for kind in [Warp, WezTerm, AppleTerminal] {
        let mut row = Row::new(kind);
        row.owner = None;
        row.state = SessionState::Launching;
        row.observation_error = Some(("presence", 1));
        let mut chain = vec![];
        let mut calls = OWNERLESS.to_vec();
        if kind == AppleTerminal {
            calls.push("instances");
        } else {
            chain.push(
                "could not prove absence of the ownerless startup surface; no close was sent",
            );
        }
        chain.extend(["injected presence failure", "injected presence cause"]);
        calls.push("presence");
        row.check_error_chain(&chain, &calls);
    }
}

#[test]
fn terminal_process_birth_errors_preserve_reason_and_stop_at_observation() {
    for (path, occurrence, calls) in [
        (
            "resumable",
            1,
            with(&OWNED, &["resumable", "resume-pid-alive", "owner", "birth"]),
        ),
        (
            "live",
            1,
            with(&OWNED, &["resumable", "owner", "mac-live", "birth"]),
        ),
        (
            "dead",
            1,
            with(
                &OWNED,
                &["resumable", "owner", "mac-live", "pid-alive", "birth"],
            ),
        ),
        (
            "dead",
            2,
            with(
                &OWNED,
                &[
                    "resumable",
                    "owner",
                    "mac-live",
                    "pid-alive",
                    "birth",
                    "instances",
                    "presence",
                    "birth",
                ],
            ),
        ),
    ] {
        let mut row = Row::new(AppleTerminal);
        row.intent = path == "resumable";
        row.live = path == "live";
        row.observation_error = Some(("birth", occurrence));
        let mut chain = vec![];
        if path == "dead" {
            chain.push("could not prove absence of the dead owner's exact terminal surface; no close was sent");
        }
        // Incarnation classification preserves the reason as one string.
        chain.push("injected birth failure: injected birth cause");
        row.check_error_chain(&chain, &calls);
    }
}

#[test]
fn terminal_instance_errors_preserve_chain_and_stop_at_observation() {
    for (path, recorded, occurrence, calls) in [
        (
            "resumable",
            true,
            1,
            with(
                &OWNED,
                &[
                    "resumable",
                    "resume-pid-alive",
                    "owner",
                    "birth",
                    "instances",
                ],
            ),
        ),
        (
            "resumable",
            false,
            1,
            with(
                &OWNED,
                &["resumable", "resume-pid-alive", "owner", "instances"],
            ),
        ),
        (
            "resumable",
            false,
            2,
            with(
                &OWNED,
                &[
                    "resumable",
                    "resume-pid-alive",
                    "owner",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
        ),
        (
            "live",
            true,
            1,
            with(
                &OWNED,
                &["resumable", "owner", "mac-live", "birth", "instances"],
            ),
        ),
        (
            "dead",
            true,
            1,
            with(
                &OWNED,
                &[
                    "resumable",
                    "owner",
                    "mac-live",
                    "pid-alive",
                    "birth",
                    "instances",
                ],
            ),
        ),
        (
            "dead",
            true,
            2,
            with(
                &OWNED,
                &[
                    "resumable",
                    "owner",
                    "mac-live",
                    "pid-alive",
                    "birth",
                    "instances",
                    "presence",
                    "birth",
                    "instances",
                ],
            ),
        ),
        (
            "dead",
            false,
            1,
            with(
                &OWNED,
                &["resumable", "owner", "mac-live", "pid-alive", "instances"],
            ),
        ),
        (
            "dead",
            false,
            2,
            with(
                &OWNED,
                &[
                    "resumable",
                    "owner",
                    "mac-live",
                    "pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
        ),
        ("startup", false, 1, with(&OWNERLESS, &["instances"])),
        (
            "startup",
            false,
            2,
            with(&OWNERLESS, &["instances", "presence", "instances"]),
        ),
    ] {
        let mut row = Row::new(AppleTerminal);
        row.intent = path == "resumable";
        row.live = path == "live";
        if !recorded {
            row.owner.as_mut().unwrap().terminal_app = None;
        }
        if path == "startup" {
            row.owner = None;
            row.state = SessionState::Launching;
        }
        row.observation_error = Some(("instances", occurrence));
        let mut chain = vec![];
        if path == "dead" {
            chain.push("could not prove absence of the dead owner's exact terminal surface; no close was sent");
        }
        chain.extend(["injected instances failure", "injected instances cause"]);
        row.check_error_chain(&chain, &calls);
    }
}

// The launch record is read before the kind is tested: a kind without failed-start
// identity reads it and nothing more of the failed start.
#[test]
fn failed_unspawned_terminal_warp_wezterm_skip_native_identity_recovery() {
    for kind in [AppleTerminal, Warp, WezTerm] {
        let mut row = Row::new(kind);
        row.state = SessionState::Failed;
        row.phase = Some(launch::Phase::Spawning);
        row.present = true;
        let mut calls = with(&OWNED, &["resumable", "owner", "mac-live", "pid-alive"]);
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        calls.push("presence");
        if kind == AppleTerminal {
            calls.extend(["birth", "instances"]);
        }
        row.check(Err(DEAD_PRESENT), &calls);
    }
}

#[test]
fn iterm_recorded_close_intent_does_not_resume() {
    let mut row = Row::new(Iterm2);
    row.intent = true;
    row.present = true;
    row.check(
        Err(DEAD_PRESENT),
        &with(
            &OWNED,
            &["resumable", "owner", "mac-live", "pid-alive", "presence"],
        ),
    );
}

#[test]
fn live_terminal_owner_without_incarnation_proceeds_to_proof() {
    let mut row = Row::new(AppleTerminal);
    row.owner.as_mut().unwrap().terminal_app = None;
    row.live = true;
    row.check(
        Ok(TerminalCloseAuthority::LiveOwner),
        &with(&OWNED, &["resumable", "owner", "mac-live", "proof"]),
    );
}

#[test]
fn live_terminal_owner_reused_or_ambiguous_incarnation_refuses_before_proof() {
    for reused in [true, false] {
        let mut row = Row::new(AppleTerminal);
        row.live = true;
        if reused {
            row.birth = Some((101, 42));
            row.check_error_chain(
                &["Terminal.app PID was reused; no close or absence authority was granted"],
                &with(&OWNED, &["resumable", "owner", "mac-live", "birth"]),
            );
        } else {
            row.instances
                .push(MacTerminalAppIdentity { pid: 789, ..app() });
            row.check_error_chain(
                &["Terminal.app scripting target is ambiguous or changed; the recorded app must be the only running instance and the exact surface handle is retained"],
                &with(
                    &OWNED,
                    &["resumable", "owner", "mac-live", "birth", "instances"],
                ),
            );
        }
    }
}

#[test]
fn dead_terminal_owner_without_incarnation_requires_stable_absence() {
    for present in [false, true] {
        let mut row = Row::new(AppleTerminal);
        row.owner.as_mut().unwrap().terminal_app = None;
        row.present = present;
        row.check(
            if present {
                Err(DEAD_PRESENT)
            } else {
                Ok(TerminalCloseAuthority::Absent)
            },
            &with(
                &OWNED,
                &[
                    "resumable",
                    "owner",
                    "mac-live",
                    "pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
        );
    }
}

#[test]
fn foreign_or_missing_surface_binding_refuses_before_surface_observation() {
    for path in ["failed-start", "dead", "intent", "startup"] {
        for binding in [Some("foreign"), None] {
            let mut row = Row::new(Ghostty);
            row.surface_binding = binding.map(str::to_owned);
            let calls = match path {
                "failed-start" => {
                    row.state = SessionState::Failed;
                    row.phase = Some(launch::Phase::Spawning);
                    FAILED_START.to_vec()
                }
                "startup" => {
                    row.owner = None;
                    row.state = SessionState::Launching;
                    OWNERLESS.to_vec()
                }
                _ => {
                    row.intent = path == "intent";
                    with(&OWNED, &["resumable", "owner", "mac-live"])
                }
            };
            row.check_error_chain(
                &[if binding.is_some() {
                    "terminal handle belongs to managed session foreign, not session-close-table"
                } else {
                    "terminal handle is missing its managed session binding"
                }],
                &calls,
            );
        }
    }
}

// The production reader changes no record on any route: the rows above whose Records
// are values run here on real Records, with the directory compared byte for byte.
#[test]
fn production_reader_decision_changes_no_record() {
    let mut rows = vec![];
    for kind in [Iterm2, AppleTerminal] {
        let mut row = Row::new(kind);
        row.live = true;
        row.proof_error = true;
        rows.push((row, Ok(TerminalCloseAuthority::OwnerTeardown)));
        let mut row = Row::new(kind);
        row.live = true;
        rows.push((row, Ok(TerminalCloseAuthority::LiveOwner)));
        let mut row = Row::new(kind);
        row.present = true;
        rows.push((row, Err(DEAD_PRESENT)));
        rows.push((Row::new(kind), Ok(TerminalCloseAuthority::Absent)));
    }
    let mut row = Row::new(Ghostty);
    row.state = SessionState::Failed;
    row.phase = Some(launch::Phase::Spawning);
    rows.push((row, Ok(TerminalCloseAuthority::SurfaceOnly)));
    let mut row = Row::new(Iterm2);
    row.owner.as_mut().unwrap().managed_session_id = Some("foreign".into());
    rows.push((row, Err(DEAD_IDENTITY)));
    for kind in [Warp, Iterm2] {
        let mut row = Row::new(kind);
        row.owner = None;
        row.state = SessionState::Launching;
        rows.push((
            row,
            if kind == Warp {
                Ok(TerminalCloseAuthority::Absent)
            } else {
                Ok(TerminalCloseAuthority::SurfaceOnly)
            },
        ));
        let mut row = Row::new(kind);
        row.owner = None;
        rows.push((
            row,
            Err("terminal close requires a live native-session owner"),
        ));
    }
    for (mut row, expected) in rows {
        row.backed = true;
        // A Record-backed row compares its directory byte for byte after the decision.
        let trace = row.check_with_error_chain(expected, None, None);
        assert!(trace.iter().any(|call| call.name == "owner-present"));
    }
}

// The Record-read contract of the decision, with what every read answered.

fn status_of(state: SessionState) -> String {
    format!("Ok(status({state}))")
}

fn owner_of(session: &str) -> String {
    format!("Ok(owner(pid 123, session Some({session:?})))")
}

#[test]
fn failed_start_reads_launch_then_status_then_owner_and_reports_each_answer() {
    let mut row = Row::new(Ghostty);
    row.state = SessionState::Failed;
    row.phase = Some(launch::Phase::Spawning);
    let trace = row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &FAILED_START);
    assert_eq!(
        trace,
        vec![
            Call::new("teardown-intent", "Ok(None)"),
            Call::new("owner-present", "Ok(true)"),
            Call::new("launch", "Ok(Some(launch(Spawning)))"),
            Call::new("status", &status_of(SessionState::Failed)),
            Call::new("owner", &owner_of(ID)),
            Call::new("owner-record", "observed(alive Some(false), matches None)"),
        ]
    );
}

#[test]
fn no_owner_reads_status_once_in_the_ownerless_branch() {
    let mut row = Row::new(Warp);
    row.owner = None;
    row.state = SessionState::Launching;
    let trace = row.check(
        Ok(TerminalCloseAuthority::Absent),
        &with(&OWNERLESS, &["presence"]),
    );
    assert_eq!(
        trace,
        vec![
            Call::new("teardown-intent", "Ok(None)"),
            Call::new("owner-present", "Ok(false)"),
            Call::new("status", &status_of(SessionState::Launching)),
            Call::new("presence", "Ok(false)"),
        ]
    );
}

// The owner is read again before attestation. The decision uses that read: a changed
// binding refuses as foreign, and a failed read stops the decision with its own error.
#[test]
fn second_owner_read_is_fresh_and_its_answer_decides() {
    for later in ["changed", "failed"] {
        let row = Row::new(Ghostty);
        let owner = row.owner.clone().unwrap();
        let handle = handle(Ghostty, row.surface_binding.clone());
        let reads = std::cell::Cell::new(0);
        let deadline = crate::native::session::close::budget::close_deadline();
        let observation = TestObservation::from_values(RecordValues {
            owner: row.owner.clone(),
            state: SessionState::Failed,
            launch: Some(launch::Record {
                schema: 1,
                claim_token: "claim-table".into(),
                deadline_unix_ms: 1,
                phase: launch::Phase::Pending,
            }),
        })
        .with_owner(|| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                return Ok(owner.clone());
            }
            if later == "failed" {
                bail!("injected owner read failure")
            }
            Ok(NativeSessionOwner {
                managed_session_id: Some("foreign".into()),
                ..owner.clone()
            })
        })
        .with_owner_record(|_| OwnerObservation {
            process_alive: Some(true),
            identity_matches: Some(true),
            error: None,
        })
        .with_presence(|| panic!("no surface observation after a refused owner"))
        .with_evidence(terminal::close_evidence(Ghostty))
        .with_pid_alive(|_| false)
        .with_mac_owner_live(|_| test_owner_attestation(false))
        .with_attest(|| panic!("dead owner"))
        .with_group_gone(|_| panic!("no teardown intent"))
        .with_proof(|| panic!("dead owner"));
        let result = observation.decide(ID, &handle, deadline);
        let error = result.unwrap_err();
        let mut expected = vec![
            Call::new("teardown-intent", "Ok(None)"),
            Call::new("owner-present", "Ok(true)"),
            Call::new("launch", "Ok(Some(launch(Pending)))"),
            Call::new("status", &status_of(SessionState::Failed)),
            Call::new("owner", &owner_of(ID)),
            Call::new(
                "owner-record",
                "observed(alive Some(true), matches Some(true))",
            ),
            Call::new("resumable", "Ok(false)"),
        ];
        if later == "failed" {
            expected.push(Call::new("owner", "Err(injected owner read failure)"));
            assert_eq!(error.to_string(), "injected owner read failure");
        } else {
            expected.extend([
                Call::new("owner", &owner_of("foreign")),
                Call::new("mac-live", "Gone(Exited)"),
                Call::new("pid-alive", "false"),
            ]);
            assert!(error.to_string().contains(DEAD_IDENTITY), "{error:#}");
        }
        assert_eq!(observation.trace(), expected, "{later}");
    }
}

// The teardown intent is the first read. One that does not validate ends the decision
// before any other Record is read; one that does is honoured without a resumable check.
#[test]
fn teardown_intent_is_read_first_and_decides_the_dead_owner_route() {
    for intent in ["damaged", "valid"] {
        let row = Row::new(Iterm2);
        let owner = row.owner.clone().unwrap();
        let handle = handle(Iterm2, row.surface_binding.clone());
        let directory = tempfile::tempdir().unwrap();
        row.write(directory.path(), &handle);
        let store = Store::open_unchecked(directory.path());
        session::close::record_terminal_teardown_intent(
            &store,
            ID,
            &handle,
            &owner,
            123,
            "the owner was ended",
        )
        .unwrap();
        if intent == "damaged" {
            let record = store.record(CoreRecord::TerminalCloseIntent);
            let mut value: serde_json::Value = record.json().unwrap();
            value["teardown"] = serde_json::json!("damaged");
            record.write_json(&value).unwrap();
        }
        let before = snapshot(directory.path());
        let deadline = crate::native::session::close::budget::close_deadline();
        let observation = TestObservation::on_records(directory.path(), ID, &handle, deadline)
            .with_presence(|| panic!("a teardown intent needs no surface observation"))
            .with_pid_alive(|_| false)
            .with_mac_owner_live(|_| test_owner_attestation(false))
            .with_resumable(|| panic!("a teardown intent is never resumable"))
            .with_attest(|| panic!("dead owner"))
            .with_group_gone(|group| {
                assert_eq!(group, 123);
                Ok(true)
            })
            .with_proof(|| panic!("dead owner"));
        let result = observation.decide(ID, &handle, deadline);
        let trace = observation.trace();
        if intent == "damaged" {
            assert!(result.is_err(), "{result:?}");
            assert_eq!(trace.len(), 1, "{trace:?}");
            assert_eq!(trace[0].name, "teardown-intent");
            assert!(trace[0].result.starts_with("Err("), "{trace:?}");
        } else {
            assert_eq!(
                result.unwrap(),
                TerminalCloseAuthority::TeardownComplete("the owner was ended".into())
            );
            assert_eq!(
                trace,
                vec![
                    Call::new(
                        "teardown-intent",
                        "Ok(Some(teardown(group 123, \"the owner was ended\")))"
                    ),
                    Call::new("owner-present", "Ok(true)"),
                    Call::new("launch", "Ok(None)"),
                    Call::new("owner", &owner_of(ID)),
                    Call::new("mac-live", "Gone(Exited)"),
                    Call::new("pid-alive", "false"),
                    Call::new("group-gone", "Ok(true)"),
                ]
            );
        }
        assert_eq!(snapshot(directory.path()), before);
    }
}

// There is no current TerminalKind that reaches "no supported native close
// identity" on macOS: AppleTerminal/Warp/WezTerm return earlier and every
// remaining kind (including WindowsConsole) has native close identity.
