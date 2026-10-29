use super::{PendingPiTurn, install_pending_turn};
use crate::native::session::{Store, turn};
#[cfg(windows)]
use crate::native::terminal;
use std::path::Path;

#[cfg(windows)]
type ConsoleSend = Box<dyn FnMut(&Path) -> terminal::TerminalSendResult>;
#[cfg(windows)]
type ReceiptRead =
    Box<dyn FnMut(&super::TurnContext<'_>, &super::input::Fingerprint) -> anyhow::Result<bool>>;
#[cfg(windows)]
type CurrentRead = Box<dyn FnMut(&super::TurnContext<'_>) -> anyhow::Result<()>>;

// A per-delivery runtime. Unscripted observations use the actual private Record
// readers; only the console and the passage of time are simulated by default.
#[cfg(windows)]
pub(super) struct InputScenario {
    pub(super) clock: crate::native::clock::FakeClock,
    pub(super) deadline: std::time::Instant,
    pub(super) receipt: Option<ReceiptRead>,
    pub(super) current: Option<CurrentRead>,
    pub(super) sends: usize,
    pub(super) reads: usize,
    send: ConsoleSend,
}

#[cfg(windows)]
impl InputScenario {
    pub(super) fn new(send: impl FnMut(&Path) -> terminal::TerminalSendResult + 'static) -> Self {
        let now = std::time::Instant::now();
        Self {
            clock: crate::native::clock::FakeClock::new(now),
            deadline: now + std::time::Duration::from_secs(30),
            receipt: None,
            current: None,
            sends: 0,
            reads: 0,
            send: Box::new(send),
        }
    }
}

#[cfg(windows)]
impl super::input::Runtime for &mut InputScenario {
    fn clock(&mut self) -> &mut dyn crate::native::clock::Clock {
        &mut self.clock
    }
    fn send(&mut self, _: &super::TurnContext<'_>, path: &Path) -> terminal::TerminalSendResult {
        self.sends += 1;
        (self.send)(path)
    }
    fn accepted(
        &mut self,
        turn: &super::TurnContext<'_>,
        fingerprint: &super::input::Fingerprint,
    ) -> anyhow::Result<bool> {
        self.reads += 1;
        match &mut self.receipt {
            Some(read) => read(turn, fingerprint),
            None => super::input::read_accepted(turn.directory, turn.claim_token, fingerprint),
        }
    }
    fn current(&mut self, turn: &super::TurnContext<'_>) -> anyhow::Result<()> {
        match &mut self.current {
            Some(read) => read(turn),
            None => super::input::current(turn),
        }
    }
}

pub(super) fn claim_pending_turn(directory: &Path) -> PendingPiTurn {
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launching_status(directory);
    install_pending_turn(directory, &token).unwrap()
}

// Installation requires a stored lifecycle; a fixture without one is a launching session.
// The record is written directly so that a later `update_status` transition from the
// fixture's own choice of state stays valid.
pub(super) fn launching_status(directory: &Path) {
    if crate::native::session::Reader::open_unchecked(directory)
        .status()
        .is_err()
    {
        stored_state(directory, &crate::native::session::SessionState::Launching);
    }
}

pub(super) fn stored_state(directory: &Path, state: &crate::native::session::SessionState) {
    crate::native::session::primitives::write_json_atomic(
        Store::open_unchecked(directory)
            .record(crate::native::session::CoreRecord::Status)
            .path(),
        &serde_json::json!({"state": state.as_str(), "updated_unix_ms": 1}),
    )
    .unwrap();
}

pub(super) fn marked(message: &str, pending: &PendingPiTurn) -> String {
    format!("{message}\n{}", pending.marker)
}
