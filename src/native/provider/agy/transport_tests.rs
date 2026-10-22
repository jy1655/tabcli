use super::delivery::{Clock, Io, Runtime};
use super::test_support::{claim_pending_turn, receipt_line};
use super::*;
use crate::native::clock::FakeClock;
use crate::native::consent::{Environment, native_key};
use crate::native::delivery::TurnKind;
use crate::native::session::primitives::{read_json, write_json_atomic, write_private};
use crate::native::session::schema::SessionManifest;
use crate::native::session::{SessionState, update_status};
use std::cell::Cell;
use std::rc::Rc;

const TRUSTED_STARTUP: &str = "I0924 16:42:24.081497     200 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)\nI0924 16:42:24.080467       1 analytics.go:187] CLI startup completed (took 230.2044ms)\n";
const REDRAW: &str = "I0924 16:42:24.127839     402 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)\n";

struct FakeRuntime {
    clock: FakeClock,
    io: FakeIo,
}
impl Runtime for FakeRuntime {
    type Clock = FakeClock;
    type Io = FakeIo;
    fn split(&mut self) -> (&mut Self::Clock, &mut Self::Io) {
        (&mut self.clock, &mut self.io)
    }
}
struct FakeIo {
    directory: PathBuf,
    home: PathBuf,
    pending: PendingAgyTurn,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    order: Vec<&'static str>,
    sent: bool,
    refuse: bool,
    now: Rc<Cell<Instant>>,
    send_finishes_at: Option<Instant>,
    expected_surface: serde_json::Value,
    expected_deadline: Instant,
}
impl Io for FakeIo {
    fn session_directory(&mut self, _: &terminal::TerminalSession) -> Result<PathBuf> {
        self.order.push("session");
        Ok(self.directory.clone())
    }
    fn environment(&mut self) -> Environment {
        self.order.push("environment");
        Environment::at_home(&self.home)
    }
    fn read_log(&mut self, path: &Path) -> Result<Option<Vec<u8>>> {
        assert_eq!(path, self.directory.join(AGY_LOG_FILE));
        self.order
            .push(if self.sent { "receipt" } else { "offset" });
        Ok(if self.sent { &self.after } else { &self.before }.clone())
    }
    fn send(
        &mut self,
        surface: &terminal::TerminalSession,
        prompt: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        assert!(!self.sent, "one paste only");
        assert_eq!(
            serde_json::to_value(surface).unwrap(),
            self.expected_surface
        );
        assert_eq!(deadline, self.expected_deadline);
        assert_eq!(prompt, self.directory.join("prompt.txt"));
        let trace = trace(&self.directory, &self.pending);
        assert_eq!(trace["outcome"], "prepared");
        assert!(trace["paste_returned_unix_ms"].is_null());
        self.order.push("send");
        self.sent = true;
        if let Some(now) = self.send_finishes_at {
            self.now.set(now);
        }
        if self.refuse {
            Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "known pre-write refusal"
            )))
        } else {
            Ok(())
        }
    }
}

fn trace(directory: &Path, pending: &PendingAgyTurn) -> serde_json::Value {
    read_json(&directory.join(format!("agy-input-{}.json", pending.claim_token))).unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
    pending: PendingAgyTurn,
    start: Instant,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-transport");
        std::fs::create_dir_all(directory.join("events")).unwrap();
        Store::open_unchecked(&directory)
            .write_manifest(&SessionManifest {
                schema: 1,
                id: "session-transport".into(),
                provider: "agy".into(),
                provider_path: "agy".into(),
                provider_version: "fixture".into(),
                workspace: directory.clone(),
                title: "runtime fixture".into(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            })
            .unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(&directory);
        let settings = root.path().join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        write_json_atomic(
            &settings,
            &serde_json::json!({"trustedWorkspaces":[native_key(&directory).unwrap()]}),
        )
        .unwrap();
        write_private(&directory.join(AGY_LOG_FILE), TRUSTED_STARTUP.as_bytes()).unwrap();
        write_private(&directory.join("prompt.txt"), b"fixture prompt").unwrap();
        Self {
            root,
            directory,
            pending,
            start: Instant::now(),
        }
    }
    fn runtime(&self) -> FakeRuntime {
        let clock = FakeClock::new(self.start);
        // Native Windows initial needs redraw; the non-Windows follow-up proves
        // readiness without one. Windows follow-ups need neither marker.
        let before = if cfg!(windows) {
            format!("{TRUSTED_STARTUP}{REDRAW}")
        } else {
            TRUSTED_STARTUP.to_owned()
        };
        let receipt = receipt_line(
            &serde_json::to_string(&format!(
                "[Agent Bridge Agy turn protocol] {}",
                self.pending.marker
            ))
            .unwrap(),
        );
        let after = format!("{before}{receipt}");
        let io = FakeIo {
            directory: self.directory.clone(),
            home: self.root.path().to_owned(),
            pending: self.pending.clone(),
            before: Some(before.into_bytes()),
            after: Some(after.into_bytes()),
            order: Vec::new(),
            sent: false,
            refuse: false,
            now: clock.shared(),
            send_finishes_at: None,
            expected_surface: serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
            expected_deadline: self.start + Duration::from_secs(20),
        };
        FakeRuntime { clock, io }
    }
    fn send(&self, runtime: FakeRuntime, initial: bool) -> (Outcome, FakeRuntime) {
        let surface = serde_json::from_value(
            serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
        )
        .unwrap();
        let mut turn = super::super::transport_test_context(
            &self.directory,
            &surface,
            if initial {
                TurnKind::Initial
            } else {
                TurnKind::FollowUp
            },
        );
        turn.claim_token = &self.pending.claim_token;
        turn.deadline = self.start + Duration::from_secs(20);
        let prepared = Prepared {
            prompt: "fixture prompt".into(),
            correlation_id: None,
        };
        let file = self.directory.join("prompt.txt");
        if initial {
            let transport = AgyInitialPaste {
                runtime: RefCell::new(runtime),
            };
            let outcome = Transport::send(&transport, &turn, &prepared, Some(&file));
            (outcome, transport.runtime.into_inner())
        } else {
            let transport = AgyPaste {
                runtime: RefCell::new(runtime),
            };
            let outcome = Transport::send(&transport, &turn, &prepared, Some(&file));
            (outcome, transport.runtime.into_inner())
        }
    }
    fn assert_retained(&self) {
        assert_eq!(
            turn::current_claim_token(&Reader::open_unchecked(&self.directory))
                .unwrap()
                .as_deref(),
            Some(self.pending.claim_token.as_str())
        );
        assert_eq!(
            read_pending_turn(&self.directory).unwrap().unwrap().marker,
            self.pending.marker
        );
    }
}

#[test]
fn native_transport_success_preserves_read_environment_and_send_order() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let offset = runtime.io.before.as_ref().unwrap().len();
    // Windows initial / macOS follow-up, through the actual transport.
    let (outcome, runtime) = fixture.send(runtime, cfg!(windows));
    assert!(matches!(outcome, Outcome::Sent));
    let mut expected = vec!["session", "environment"];
    expected.extend(std::iter::repeat_n("offset", 36));
    expected.extend(["send", "receipt"]);
    assert_eq!(runtime.io.order, expected);
    assert_eq!(runtime.clock.slept, Duration::from_millis(3500));
    let trace = trace(&fixture.directory, &fixture.pending);
    assert_eq!(trace["pre_paste_offset"], offset);
    assert_eq!(trace["outcome"], "confirmed");
    for key in [
        "prepared_unix_ms",
        "paste_started_unix_ms",
        "paste_returned_unix_ms",
        "receipt_finished_unix_ms",
    ] {
        assert!(trace[key].is_number());
    }
}

#[cfg(windows)]
#[test]
fn windows_follow_up_skips_trust_and_readiness_and_reads_offset_once() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.before = Some(b"ordinary log\n".to_vec());
    runtime.io.after = Some(
        [
            runtime.io.before.clone().unwrap(),
            receipt_line(
                &serde_json::to_string(&format!(
                    "[Agent Bridge Agy turn protocol] {}",
                    fixture.pending.marker
                ))
                .unwrap(),
            )
            .into_bytes(),
        ]
        .concat(),
    );
    std::fs::remove_file(fixture.directory.join("manifest.json")).unwrap();
    let (outcome, runtime) = fixture.send(runtime, false);
    assert!(matches!(outcome, Outcome::Sent));
    assert_eq!(runtime.io.order, ["session", "offset", "send", "receipt"]);
    assert_eq!(runtime.clock.slept, Duration::ZERO);
    assert_eq!(
        trace(&fixture.directory, &fixture.pending)["pre_paste_offset"],
        13
    );
}

#[cfg(windows)]
#[test]
fn windows_initial_withholds_paste_without_redraw() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.before = Some(TRUSTED_STARTUP.as_bytes().to_vec());
    let (outcome, runtime) = fixture.send(runtime, true);
    let Outcome::NotSent(error) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(
        error
            .to_string()
            .contains("`Full redraw completed` after `CLI startup completed`")
    );
    assert!(!runtime.io.sent);
    assert_eq!(&runtime.io.order[..2], &["session", "environment"]);
    assert!(runtime.io.order[2..].iter().all(|&event| event == "offset"));
}

#[test]
fn stale_receipt_is_ignored_and_uncertain_delivery_keeps_claim_pending_and_trace() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.before = runtime.io.after.clone();
    let (outcome, runtime) = fixture.send(runtime, false);
    let Outcome::Uncertain(error) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(
        error
            .to_string()
            .contains("no HandleUserInput receipt in the 0 bytes appended")
    );
    assert_eq!(
        runtime
            .io
            .order
            .iter()
            .filter(|&&event| event == "send")
            .count(),
        1
    );
    fixture.assert_retained();
    let trace = trace(&fixture.directory, &fixture.pending);
    assert_eq!(trace["outcome"], "delivery-uncertain");
    assert_eq!(trace["error"], error.to_string());
}

#[test]
fn missing_log_is_not_sent() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.before = None;
    let (outcome, runtime) = fixture.send(runtime, false);
    assert!(matches!(outcome, Outcome::NotSent(_)));
    assert!(!runtime.io.sent);
    if cfg!(windows) {
        assert_eq!(runtime.io.order, ["session", "offset"]);
    }
}

#[test]
fn trust_failure_precedes_pending_and_readiness() {
    let fixture = Fixture::new();
    // No pending record either: the reported failure must still be trust.
    std::fs::remove_file(fixture.directory.join(PENDING_TURN_FILE)).unwrap();
    std::fs::remove_file(fixture.directory.join(AGY_LOG_FILE)).unwrap();
    write_private(
        &fixture.directory.join(AGY_LOG_FILE),
        b"no per-process trust evidence\n",
    )
    .unwrap();
    let (outcome, runtime) = fixture.send(fixture.runtime(), cfg!(windows));
    let Outcome::NotSent(error) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(
        error
            .to_string()
            .contains("this session's agy.log does not show")
    );
    assert_eq!(runtime.io.order, ["session", "environment"]);
}

#[test]
fn missing_pending_stops_after_trust_before_readiness() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.directory.join(PENDING_TURN_FILE)).unwrap();
    let (outcome, runtime) = fixture.send(fixture.runtime(), cfg!(windows));
    let Outcome::NotSent(error) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(error.to_string(), "Agy turn correlation state is missing");
    assert_eq!(runtime.io.order, ["session", "environment"]);
    assert_eq!(runtime.clock.slept, Duration::ZERO);
}

#[test]
fn pre_write_refusal_skips_receipt_confirmation() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.refuse = true;
    let (outcome, runtime) = fixture.send(runtime, false);
    assert!(matches!(outcome, Outcome::NotSent(_)));
    assert_eq!(runtime.io.order.last(), Some(&"send"));
    assert!(!runtime.io.order.contains(&"receipt"));
    let trace = trace(&fixture.directory, &fixture.pending);
    assert_eq!(trace["outcome"], "not-sent");
    assert_eq!(trace["error"], "known pre-write refusal");
}

#[test]
fn receipt_window_starts_after_send_advances_time() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    runtime.io.after = runtime.io.before.clone();
    runtime.io.send_finishes_at = Some(fixture.start + Duration::from_secs(18));
    let (outcome, mut runtime) = fixture.send(runtime, false);
    let Outcome::Uncertain(error) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(
        error
            .to_string()
            .contains("deadline ended the receipt window after 2 of the 60 second window")
    );
    assert_eq!(runtime.clock.now(), fixture.start + Duration::from_secs(20));
    assert_eq!(
        runtime
            .io
            .order
            .iter()
            .filter(|&&event| event == "send")
            .count(),
        1
    );
    fixture.assert_retained();
    assert_eq!(
        trace(&fixture.directory, &fixture.pending)["outcome"],
        "delivery-uncertain"
    );
}
