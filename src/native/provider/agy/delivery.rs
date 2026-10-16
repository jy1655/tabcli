use super::readiness::{ReadinessTiming, wait_for_startup_readiness_with};
use super::receipt::confirm_input_receipt_with;
use super::trust::wait_for_workspace_trust;
use super::{
    AGY_LOG_FILE, INPUT_RECEIPT_WINDOW, PendingAgyTurn, STARTUP_POLL_INTERVAL, read_pending_turn,
};
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
pub(super) use crate::native::consent::{Clock, SystemClock};

pub(super) fn read_log_bytes(log_path: &Path) -> Result<Option<Vec<u8>>> {
    RecordReader::at(log_path).bytes()
}

// Only preparation can produce a paste-ready turn. Consuming it issues one paste
// and checks the receipt against the offset from the read that passed readiness.
struct PreparedTerminalTurn<'a> {
    directory: &'a Path,
    pending: PendingAgyTurn,
    pre_paste_len: usize,
}

impl<'a> PreparedTerminalTurn<'a> {
    fn prepare_with(
        directory: &'a Path,
        trust: impl FnOnce() -> Result<()>,
        readiness: impl FnOnce() -> Result<usize>,
    ) -> Result<Self> {
        trust()?;
        let pending =
            read_pending_turn(directory)?.context("Agy turn correlation state is missing")?;
        let pre_paste_len = readiness()?;
        Ok(Self {
            directory,
            pending,
            pre_paste_len,
        })
    }

    fn deliver(
        self,
        terminal: &str,
        send: impl FnOnce() -> terminal::TerminalSendResult,
        confirm: impl FnOnce(&PendingAgyTurn, usize) -> terminal::TerminalSendResult,
    ) -> terminal::TerminalSendResult {
        trace_terminal_delivery(
            self.directory,
            &self.pending,
            self.pre_paste_len,
            terminal,
            send,
            || confirm(&self.pending, self.pre_paste_len),
        )
    }
}

// A Windows follow-up has already passed startup trust/readiness in its initial
// paste; all other pastes wait for both gates here before preparing the turn.
pub(super) fn deliver_terminal_turn(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    readiness: Option<ReadinessTiming>,
) -> terminal::TerminalSendResult {
    use terminal::TerminalSendFailure;
    let directory = session_directory_of(session).map_err(TerminalSendFailure::not_sent)?;
    let log_path = Reader::open_unchecked(&directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let prepared = PreparedTerminalTurn::prepare_with(
        &directory,
        || {
            if readiness.is_some() {
                wait_for_workspace_trust(
                    &directory,
                    deadline,
                    &crate::native::consent::Environment::current(),
                )?;
            }
            Ok(())
        },
        || {
            if let Some(timing) = readiness {
                // No later log read may replace the gate's receipt offset.
                wait_for_startup_readiness_with(
                    &mut || read_log_bytes(&log_path),
                    deadline,
                    timing,
                    STARTUP_POLL_INTERVAL,
                    &mut SystemClock,
                )
            } else {
                let log = read_log_bytes(&log_path)
                    .context("Agy log could not be read before the console paste")?;
                follow_up_pre_paste_offset(log.as_deref())
            }
        },
    )
    .map_err(TerminalSendFailure::not_sent)?;
    prepared.deliver(
        session.kind.as_str(),
        || terminal::send_file(session, prompt_path, deadline),
        |pending, pre_paste_len| {
            // The composer after an unconfirmed paste is unknown; never paste again.
            confirm_input_receipt_with(
                &mut || read_log_bytes(&log_path),
                pending,
                pre_paste_len,
                Instant::now(),
                deadline,
                INPUT_RECEIPT_POLL_INTERVAL,
                &mut SystemClock,
            )
        },
    )
}

// Request-scoped timing evidence for #48. A missing receipt alone cannot locate
// the failure between terminal injection, a late reload, and Agy's input loop.
// Record actual dispatch times and the offset used by the receipt check without
// copying prompt text. Diagnostic write failures never cancel or retry delivery.
fn trace_terminal_delivery<S, C>(
    directory: &Path,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    terminal: &str,
    send: S,
    confirm: C,
) -> terminal::TerminalSendResult
where
    S: FnOnce() -> terminal::TerminalSendResult,
    C: FnOnce() -> terminal::TerminalSendResult,
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
    let result = send();
    trace["paste_returned_unix_ms"] =
        serde_json::json!(crate::native::session::primitives::unix_ms());
    let result = result.and_then(|()| confirm());
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

// Complete log lines only: a line without its terminating newline may still be
// written, so it is neither a marker nor a receipt yet. Terminal escape sequences and
// carriage returns are dropped so a marker is recognised through console noise.
pub(super) fn complete_log_lines(log: &[u8]) -> impl Iterator<Item = String> + '_ {
    let complete = match log.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &log[..end],
        None => &log[..0],
    };
    complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| strip_terminal_noise(&String::from_utf8_lossy(line)))
}

pub(super) fn strip_terminal_noise(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut clean = String::with_capacity(line.len());
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '\u{1b}' => {
                index += 1;
                match characters.get(index) {
                    Some('[') => {
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            index += 1;
                            if ('\u{40}'..='\u{7e}').contains(next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC ends with BEL or with the string terminator `ESC \`.
                        // Any other escape ends the OSC and is processed on its own.
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            if *next == '\u{7}' {
                                index += 1;
                                break;
                            }
                            if *next == '\u{1b}' {
                                if characters.get(index + 1) == Some(&'\\') {
                                    index += 2;
                                }
                                break;
                            }
                            index += 1;
                        }
                    }
                    Some(_) => index += 1,
                    None => {}
                }
            }
            '\r' => index += 1,
            other => {
                clean.push(other);
                index += 1;
            }
        }
    }
    clean
}

// The glog `[IWEF]MMDD HH:MM:SS.uuuuuu` stamp of a line as an offset from an
// arbitrary epoch (months counted as 31 days); only the difference between two
// stamps of the same log is meaningful, and a difference across a year boundary is
// not. Agy stamps in local time. A line without a stamp (`CLI ready for user
// input`, a file header, console noise) yields `None`.
pub(super) fn glog_timestamp(line: &str) -> Option<Duration> {
    let bytes = line.as_bytes();
    if !matches!(bytes.first(), Some(b'I' | b'W' | b'E' | b'F')) || bytes.get(5) != Some(&b' ') {
        return None;
    }
    let date = line.get(1..5)?;
    let month: u64 = date.get(..2)?.parse().ok()?;
    let day: u64 = date.get(2..)?.parse().ok()?;
    let stamp = line.get(6..21)?;
    let (hours, rest) = stamp.split_once(':')?;
    let (minutes, seconds) = rest.split_once(':')?;
    let (seconds, micros) = seconds.split_once('.')?;
    if micros.len() != 6 {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: u64 = seconds.parse().ok()?;
    let micros: u64 = micros.parse().ok()?;
    Some(
        Duration::from_secs((month * 31 + day) * 86_400 + hours * 3600 + minutes * 60 + seconds)
            + Duration::from_micros(micros),
    )
}

#[cfg(test)]
mod tests;
