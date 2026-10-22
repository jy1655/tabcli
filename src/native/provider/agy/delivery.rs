use super::readiness::{ReadinessTiming, wait_for_startup_readiness_with};
use super::receipt::confirm_input_receipt_with;
use super::trust::wait_for_workspace_trust;
use super::{
    AGY_LOG_FILE, INPUT_RECEIPT_WINDOW, PendingAgyTurn, STARTUP_POLL_INTERVAL, read_pending_turn,
};
use crate::native::consent::Environment;
use crate::native::session::{Reader, RecordReader, Store};
use crate::native::terminal;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Paste only after exact trust, per-process customization load, startup and quiet log evidence.
// Windows also requires redraw; macOS launch arguments need no paste gate.
// Replaced logs reset readiness; the qualifying read's byte length bounds the receipt search.
// Only a complete framed HandleUserInput marker after that offset proves delivery.
// Later activity never revokes a receipt; missing evidence stays uncertain and is never re-pasted.
// Keep the claim and status.error on uncertainty; cleanup belongs to an explicit caller action.
// Replace these gates when Agy exposes input/acceptance; the transcript monitor is independent.
// See docs/verification/2026-10-01-bug-triage.md for the investigation.

const INPUT_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

// The clock is consent's, shared with the trust wait; the readiness and receipt gates
// take it so that tests run them on simulated time.
pub(super) use crate::native::clock::{Clock, SystemClock};

pub(super) fn read_log_bytes(log_path: &Path) -> Result<Option<Vec<u8>>> {
    RecordReader::at(log_path).bytes()
}

// Agy owns these effects; the gates borrow clock and I/O independently so each
// poll remains a fresh read. Trust deliberately keeps its real store/log reads.
pub(super) trait Io {
    fn session_directory(&mut self, session: &terminal::TerminalSession) -> Result<PathBuf>;
    fn environment(&mut self) -> Environment;
    fn read_log(&mut self, path: &Path) -> Result<Option<Vec<u8>>>;
    fn send(
        &mut self,
        session: &terminal::TerminalSession,
        prompt: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult;
}

pub(super) trait Runtime {
    type Clock: Clock;
    type Io: Io;
    fn split(&mut self) -> (&mut Self::Clock, &mut Self::Io);
}

pub(super) struct Console {
    clock: SystemClock,
    io: ConsoleIo,
}

impl Console {
    pub(super) fn new() -> Self {
        Self {
            clock: SystemClock,
            io: ConsoleIo,
        }
    }
}

impl Runtime for Console {
    type Clock = SystemClock;
    type Io = ConsoleIo;
    fn split(&mut self) -> (&mut Self::Clock, &mut Self::Io) {
        (&mut self.clock, &mut self.io)
    }
}

pub(super) struct ConsoleIo;
impl Io for ConsoleIo {
    fn session_directory(&mut self, session: &terminal::TerminalSession) -> Result<PathBuf> {
        session_directory_of(session)
    }
    fn environment(&mut self) -> Environment {
        Environment::current()
    }
    fn read_log(&mut self, path: &Path) -> Result<Option<Vec<u8>>> {
        read_log_bytes(path)
    }
    fn send(
        &mut self,
        session: &terminal::TerminalSession,
        prompt: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        terminal::send_file(session, prompt, deadline)
    }
}

// Only preparation can produce a paste-ready turn. Consuming it issues one paste
// and checks the receipt against the offset from the read that passed readiness.
struct PreparedTerminalTurn<'a> {
    directory: &'a Path,
    pending: PendingAgyTurn,
    pre_paste_len: usize,
}

impl<'a> PreparedTerminalTurn<'a> {
    fn prepare_with<R>(
        directory: &'a Path,
        runtime: &mut R,
        trust: impl FnOnce(&mut R) -> Result<()>,
        readiness: impl FnOnce(&mut R) -> Result<usize>,
    ) -> Result<Self> {
        trust(runtime)?;
        let pending =
            read_pending_turn(directory)?.context("Agy turn correlation state is missing")?;
        let pre_paste_len = readiness(runtime)?;
        Ok(Self {
            directory,
            pending,
            pre_paste_len,
        })
    }

    fn deliver<R>(
        self,
        terminal: &str,
        runtime: &mut R,
        send: impl FnOnce(&mut R) -> terminal::TerminalSendResult,
        confirm: impl FnOnce(&mut R, &PendingAgyTurn, usize) -> terminal::TerminalSendResult,
    ) -> terminal::TerminalSendResult {
        trace_terminal_delivery(
            self.directory,
            &self.pending,
            self.pre_paste_len,
            terminal,
            runtime,
            send,
            |runtime| confirm(runtime, &self.pending, self.pre_paste_len),
        )
    }
}

// A Windows follow-up has already passed startup trust/readiness in its initial
// paste; all other pastes wait for both gates here before preparing the turn.
pub(super) fn deliver_terminal_turn<R: Runtime>(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    readiness: Option<ReadinessTiming>,
    runtime: &mut R,
) -> terminal::TerminalSendResult {
    use terminal::TerminalSendFailure;
    let directory = runtime
        .split()
        .1
        .session_directory(session)
        .map_err(TerminalSendFailure::not_sent)?;
    let log_path = Reader::open_unchecked(&directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let prepared = PreparedTerminalTurn::prepare_with(
        &directory,
        runtime,
        |runtime| {
            if readiness.is_some() {
                let (clock, io) = runtime.split();
                wait_for_workspace_trust(&directory, deadline, &io.environment(), clock)?;
            }
            Ok(())
        },
        |runtime| {
            let (clock, io) = runtime.split();
            if let Some(timing) = readiness {
                // No later log read may replace the gate's receipt offset.
                wait_for_startup_readiness_with(
                    &mut || io.read_log(&log_path),
                    deadline,
                    timing,
                    STARTUP_POLL_INTERVAL,
                    clock,
                )
            } else {
                let log = io
                    .read_log(&log_path)
                    .context("Agy log could not be read before the console paste")?;
                follow_up_pre_paste_offset(log.as_deref())
            }
        },
    )
    .map_err(TerminalSendFailure::not_sent)?;
    prepared.deliver(
        session.kind.as_str(),
        runtime,
        |runtime| runtime.split().1.send(session, prompt_path, deadline),
        |runtime, pending, pre_paste_len| {
            let (clock, io) = runtime.split();
            // The composer after an unconfirmed paste is unknown; never paste again.
            confirm_input_receipt_with(
                &mut || io.read_log(&log_path),
                pending,
                pre_paste_len,
                clock.now(),
                deadline,
                INPUT_RECEIPT_POLL_INTERVAL,
                clock,
            )
        },
    )
}

// Request-scoped timing evidence for #48. A missing receipt alone cannot locate
// the failure between terminal injection, a late reload, and Agy's input loop.
// Record actual dispatch times and the offset used by the receipt check without
// copying prompt text. Diagnostic write failures never cancel or retry delivery.
fn trace_terminal_delivery<R, S, C>(
    directory: &Path,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    terminal: &str,
    runtime: &mut R,
    send: S,
    confirm: C,
) -> terminal::TerminalSendResult
where
    S: FnOnce(&mut R) -> terminal::TerminalSendResult,
    C: FnOnce(&mut R) -> terminal::TerminalSendResult,
{
    let record = Store::open_unchecked(directory)
        .private(&format!("agy-input-{}.json", pending.claim_token));
    let mut trace = serde_json::json!({
        "schema":1, "claim_token":pending.claim_token, "terminal":terminal,
        "pre_paste_offset":pre_paste_len, "prepared_unix_ms":crate::native::session::primitives::unix_ms(),
        "outcome":"prepared", "paste_started_unix_ms":null,
        "paste_returned_unix_ms":null, "receipt_finished_unix_ms":null, "error":null,
    });
    let _ = record.write_json(&trace);
    trace["paste_started_unix_ms"] =
        serde_json::json!(crate::native::session::primitives::unix_ms());
    let result = send(runtime);
    trace["paste_returned_unix_ms"] =
        serde_json::json!(crate::native::session::primitives::unix_ms());
    let result = result.and_then(|()| confirm(runtime));
    trace["receipt_finished_unix_ms"] =
        serde_json::json!(crate::native::session::primitives::unix_ms());
    trace["outcome"] = serde_json::json!(match &result {
        Ok(()) => "confirmed",
        Err(error) if error.delivery_may_have_occurred() => "delivery-uncertain",
        Err(_) => "not-sent",
    });
    if let Err(error) = &result {
        trace["error"] = serde_json::json!(error.error().to_string());
    }
    let _ = record.write_json(&trace);
    result
}

// A follow-up is pasted into a session whose Agy has been logging since launch, so
// the log it will write the receipt to already exists. A missing log is a
// discontinuity, never offset zero: pasting against it could only end uncertain.
fn follow_up_pre_paste_offset(log: Option<&[u8]>) -> Result<usize> {
    log.map(<[u8]>::len).context(
        "agy.log is missing before the console paste, so no input receipt could be observed; the follow-up was not pasted",
    )
}

fn session_directory_of(session: &terminal::TerminalSession) -> Result<PathBuf> {
    let id = session
        .managed_session_id
        .as_deref()
        .context("Agy terminal handle is missing its managed session binding")?;
    Reader::session_directory(id)
}

pub(super) fn input_receipt_window_end(now: Instant, deadline: Instant) -> Instant {
    now.checked_add(INPUT_RECEIPT_WINDOW)
        .map_or(deadline, |end| end.min(deadline))
}

#[cfg(test)]
mod tests;
