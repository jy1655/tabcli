//! The second Delivery adapter: a fake Transport and fake Checks over a real Store and
//! Claim, shared by the Delivery tests and the Turn-run tests.
use super::super::session::{CoreRecord, Store, update_status};
use super::*;
use std::{cell::RefCell, fs, rc::Rc};
pub(in crate::native) type Calls = Rc<RefCell<Vec<&'static str>>>;
pub(in crate::native) struct Fake {
    pub(in crate::native) calls: Calls,
    pub(in crate::native) surface: bool,
    pub(in crate::native) result: &'static str,
    pub(in crate::native) prepare_fails: bool,
    pub(in crate::native) on_send: RefCell<Option<Box<dyn FnOnce()>>>,
    pub(in crate::native) files_at_discard: RefCell<Vec<bool>>,
}
impl Transport for Fake {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "fake",
            readiness: Readiness::AfterLaunch {
                ready_delay: Duration::ZERO,
            },
            surface: self.surface.then_some(SurfaceInput {
                submit_count: 2,
                newline: terminal::ConsoleNewline::LineFeed,
            }),
        }
    }
    fn prepare(&self, turn: &TurnContext<'_>) -> Result<Prepared> {
        self.calls.borrow_mut().push("prepare");
        if self.prepare_fails {
            anyhow::bail!("prepare cause");
        }
        Ok(Prepared {
            prompt: turn.prompt.into(),
            correlation_id: Some("correlation".into()),
        })
    }
    fn send(&self, _: &TurnContext<'_>, _: &Prepared, file: Option<&Path>) -> Outcome {
        self.calls.borrow_mut().push("send");
        if let Some(f) = self.on_send.borrow_mut().take() {
            f();
        }
        assert_eq!(file.is_some(), self.surface);
        if let Some(file) = file {
            assert!(file.is_file());
        }
        match self.result {
            "sent" => Outcome::Sent,
            "not sent" => Outcome::NotSent(anyhow::anyhow!("send cause")),
            _ => Outcome::Uncertain(anyhow::anyhow!("send cause")),
        }
    }
    fn discard_prepared(&self, turn: &TurnContext<'_>, prepared: Option<&Prepared>) {
        self.files_at_discard
            .borrow_mut()
            .push(fs::read_dir(turn.directory).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pending-prompt-")
            }));
        self.calls.borrow_mut().push(if prepared.is_some() {
            "discard some"
        } else {
            "discard none"
        });
    }
}
pub(in crate::native) struct Check {
    pub(in crate::native) calls: Calls,
    pub(in crate::native) fail: &'static str,
    pub(in crate::native) before_begin: RefCell<Option<Box<dyn FnOnce()>>>,
}
impl Checks for Check {
    fn ready(&self, _: &DeliveryContext<'_>, _: Duration) -> Result<()> {
        self.calls.borrow_mut().push("ready");
        Ok(())
    }
    fn ownership(&self, _: &DeliveryContext<'_>) -> Result<()> {
        self.calls.borrow_mut().push("ownership");
        if self.fail == "ownership" {
            anyhow::bail!("ownership cause");
        }
        Ok(())
    }
    fn remaining(&self, _: &DeliveryContext<'_>) -> Result<Duration> {
        self.calls.borrow_mut().push("remaining");
        if let Some(f) = self.before_begin.borrow_mut().take() {
            f();
        }
        if self.fail == "timeout" {
            anyhow::bail!("timed out after 30 seconds");
        }
        Ok(Duration::from_secs(20))
    }
    fn budget(&self, _: &DeliveryContext<'_>, _: usize, _: Duration) -> Result<()> {
        self.calls.borrow_mut().push("budget");
        if self.fail == "budget" {
            anyhow::bail!("budget cause");
        }
        Ok(())
    }
}
pub(in crate::native) struct Fixture {
    pub(in crate::native) _dir: tempfile::TempDir,
    pub(in crate::native) store: Store,
    pub(in crate::native) surface: terminal::TerminalSession,
    pub(in crate::native) fake: Fake,
    pub(in crate::native) checks: Check,
}
impl Fixture {
    pub(in crate::native) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(dir.path());
        fs::create_dir(store.record(CoreRecord::Events).path()).unwrap();
        let calls = Calls::default();
        Self {
            _dir: dir,
            store,
            surface: serde_json::from_value(
                serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
            )
            .unwrap(),
            fake: Fake {
                calls: calls.clone(),
                surface: true,
                result: "sent",
                prepare_fails: false,
                on_send: RefCell::new(None),
                files_at_discard: RefCell::new(Vec::new()),
            },
            checks: Check {
                calls,
                fail: "",
                before_begin: RefCell::new(None),
            },
        }
    }
    /// An initial Claim over a preserved prompt, or a follow-up Claim whose `claim_ready`
    /// baseline is discarded.
    pub(in crate::native) fn claim(&self, initial: bool) -> turn::Claim {
        if initial {
            update_status(
                self.store.directory(),
                SessionState::AwaitingInitialInput,
                None,
                None,
            )
            .unwrap();
            self.store
                .record(CoreRecord::InitialPrompt)
                .write_private(b"prompt")
                .unwrap();
            turn::claim(&self.store, &[]).unwrap()
        } else {
            self.claim_ready().0
        }
    }
    /// A follow-up Claim with its `claim_ready` baseline.
    pub(in crate::native) fn claim_ready(&self) -> (turn::Claim, usize) {
        update_status(self.store.directory(), SessionState::Ready, None, None).unwrap();
        turn::claim_ready(&self.store, "session-test", &[]).unwrap()
    }
    pub(in crate::native) fn context(&self) -> DeliveryContext<'_> {
        DeliveryContext {
            directory: self.store.directory(),
            session_id: "session-test",
            surface: &self.surface,
            provider_path: Path::new("provider"),
            deadline: Instant::now() + Duration::from_secs(30),
            requested: Duration::from_secs(30),
        }
    }
    pub(in crate::native) fn follow(&self, claim: turn::Claim) -> Result<Delivered> {
        deliver_with(
            claim,
            Turn::FollowUp { prompt: "prompt" },
            self.context(),
            &self.fake,
            &self.checks,
        )
    }
    pub(in crate::native) fn calls(&self) -> Vec<&'static str> {
        self.fake.calls.borrow().clone()
    }
}
