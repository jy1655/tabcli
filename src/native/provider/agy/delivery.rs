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
use std::thread;
use std::time::{Duration, Instant};

// Terminal paste delivery verification (Agy adapter fallback, issues #43 and #48).
//
// Agy has no first-party input API for a running interactive session and no
// ready/accepted signal for a turn, so the adapter pastes the framed prompt into the
// managed terminal. Until Agy exposes either a first-party input path or a per-turn
// ready/accepted signal, this adapter reads Agy's own trust store and `--log-file`
// output (glog lines) as the only available evidence.
//
// What loses a paste (root cause, 2026-10-01): Agy keeps its workspace-trust dialog
// over the composer until the exact workspace is in its own trust store, and it runs
// an argument-delivered initial prompt behind the dialog. A paste onto the dialog is
// discarded and its Enter confirms the preselected "Yes, I trust this folder"; Agy
// then logs `Reloading system slash commands and skills` with three companion lines
// (356 bytes; `TrustWorkspace` -> `AddWorkspaceDir` -> `reloadWorkspaceCustomizations`
// in the 1.2.14 binary) and no `HandleUserInput`, and the session is left `working`
// at an empty composer. Reproduced on demand with Agy 1.2.14 on macOS (session-U2yPxX:
// dialog on screen, paste, those 356 bytes 0.5 s later, the workspace added to
// `trustedWorkspaces`, no receipt). In more than 100 Agy logs on that Mac
// (2026-09-10 to 2026-10-01) the reload appears only in sessions whose workspace is
// in the trust store, and in none of the sessions whose workspace never got there.
// The "deferred skills reload" of the 2026-09-24 notes below is this reload: it
// followed every lost paste by at most 1.4 s because the paste's own Enter caused
// it, and it never came in a workspace that was already trusted. It is not a startup
// timer, so no window can stand in for the trust evidence.
//
// - workspace trust: no paste before Agy's store lists the exact workspace and this
//   session's own log shows the workspace customization load, the per-process
//   evidence that its dialog is gone (`wait_for_workspace_trust_with`). The macOS
//   follow-up and the native Windows initial paste both wait for it.
// - readiness gate: `CLI startup completed` (analytics.go), on the Windows console
//   at least one `Full redraw completed` (manager.go) line after it, and a quiet
//   period in which no `Reloading system slash commands` line (with or without "and
//   skills"), no `Full redraw completed` line, and no `hooks_manager.go` line
//   arrives, measured on the gate's clock from the first read that showed the newest
//   such line. The gate is ready on the first read at which every condition holds,
//   and it never waits for a hooks completion after a reload; the startup reload of
//   session-IQHEwf had none.
//   No rule waits for the trust reload any more. The rules of 0.0.7 and 0.0.8 held
//   the paste back for a "deferred reload window" (45 s on the Windows console after
//   20 and 35 s had proved too short, 60 s on macOS) in which a `Reloading system
//   slash commands and skills` line stamped at least 1 s after `CLI startup
//   completed` ended the wait.
//   That line is the trust reload, logged when a paste's Enter approved the dialog
//   (the session-IEKjtC paste at +9.5 s, 2026-09-24 17:47, the session-fMqSQc paste
//   at +12 s, 2026-09-24 16:41, the session-ql5TVc paste at +20.1 s, 2026-09-24
//   18:48, and the session-uqraap paste at +35 s, 2026-09-24 20:44), so it never came
//   before a paste and never comes in a workspace trusted before launch, where the
//   window only delayed the paste (session-IQHEwf, 2026-09-24 17:20, logged its
//   startup reload before `CLI startup completed`, never logged another, and went
//   silent). The trust evidence is checked first instead. macOS dropped its window
//   when the cause was found. The Windows console dropped it once native Windows was
//   verified again (2026-10-01): session-jkxi48 (Agy 1.2.10, the 45 s rule) logged
//   the customization load 58 ms after startup and no trust reload before its paste
//   at +45 s, and session-C2fMs7 and session-uTpvwY (Agy 1.2.14, no window) pasted
//   right after the quiet period, 8.8 and 7.9 s after startup, and logged their
//   receipts 1.2 and 1.4 s later. `ReadinessTiming` still carries a window so that
//   the tests can replay the recorded sessions against the former rules; no rule in
//   this file sets one, and it can go together with those replays.
//   The gate also keeps the byte length and a digest of every byte of the newest
//   read (`LogContinuity`): a log that disappears, shrinks, or no longer reproduces
//   that digest over the observed length was replaced or rotated, so every
//   settlement instant is discarded and the quiet period is re-measured from the new
//   content. Any number of such restarts is tolerated within the deadline: the gate
//   fails the paste as `not_sent` only when the deadline passes, and the report then
//   lists every discontinuity in order (review round 6, replacing round 5's
//   leading-bytes check and its failure on the second discontinuity).
// - input receipt: a complete `HandleUserInput called with text: "..."` line
//   (input_loop.go) that starts after the byte length of agy.log observed
//   immediately before the paste and whose text carries the adapter's framing and
//   the complete pending turn marker.
//
// Delivery classification after a paste (issue #43 review):
//
// - delivered: such a receipt line exists after the pre-paste offset. Nothing
//   logged after the receipt revokes it. Agy 1.2.10 logs `Reloading system slash
//   commands and skills` a few milliseconds after `HandleUserInput` and the
//   conversation-start lines (`Starting new conversation`, `Created conversation`):
//   session-udT6uY logged it 15 ms after the receipt (2026-09-24 16:42:50.228), and
//   both delivered live asks of round 7 showed the same. That is the reload the new
//   conversation triggers, not the trust reload, so it is never evidence that the
//   paste was lost. `observe_input_receipt` looks only for the receipt, and
//   `confirm_input_receipt_with` returns on the first read that yields `Delivered`,
//   so a later reload cannot flip a delivered classification;
// - delivery-uncertain: everything else. Non-delivery would have to be proven by a
//   line Agy logs after draining its console input without a receipt, and the real
//   logs contain no such marker (session-fMqSQc, 2026-09-24: after the discarded
//   paste Agy logged only the trust reload and then nothing for a minute), so a
//   missing receipt at the end of the window, a deadline-capped window, an
//   unreadable or missing log, a log shorter than the pre-paste offset (rotated or
//   truncated), and a partial trailing line all stay uncertain and never `not_sent`.
//   The paste is never repeated. A lost paste therefore ends as delivery-uncertain:
//   the launcher keeps the turn claim, leaves the session `working`, and writes the
//   receipt error to `status.error` (`Claim::settle_delivery` for both
//   the initial prompt and `tell`).
//   The caller must inspect that error (or `doctor <session>`) and then close the
//   session with `close-session --explicit` or launch a new one; the bridge never
//   re-pastes or cleans up on its own.
//
// macOS follow-up delivery (iTerm2/Terminal.app paste): the initial prompt is a
// `--prompt-interactive` argument, so the first paste is the first `tell`. Three
// differences from the Windows console:
//
// - The trust evidence is checked at the follow-up, not at launch: the argument
//   prompt completes behind the dialog, so an `ask` in a workspace no provider has
//   approved returns its result while the dialog is still up. Issue #48 was that
//   state: the former 60 s window ended, the paste landed on the dialog, and the
//   receipt check saw only the 356 bytes of the trust reload.
// - There is no deferred-reload window (`MACOS_FOLLOW_UP_READINESS_TIMING`): once the
//   trust evidence holds nothing is pending, and a follow-up in a workspace trusted
//   before launch no longer waits 60 s for a reload that never comes.
// - Agy on macOS logs no `Full redraw completed` line at all (session-QMFk6F and
//   session-7IgCnx, this machine), so the macOS rule does not require the redraw
//   marker. The argument prompt starts a conversation a few seconds after startup,
//   and the `Reloading system slash commands and skills` line a few milliseconds
//   after `Starting new conversation` (7 ms in session-QMFk6F at +2.9 s, 26 ms in
//   session-7IgCnx at +2.7 s) is the conversation reload: an activity line for the
//   quiet period, and never the trust reload on either platform
//   (`CONVERSATION_RELOAD_MAX_LATENCY`).
//
// The receipt is the same Go-quoted `HandleUserInput` line: a multi-line paste
// through iTerm2 is logged as one record with `\n` escapes and the complete marker
// (session-QMFk6F, 21:42:19). It carries the macOS framing header instead of the
// Windows one, and `receipt_matches` accepts either adapter-owned framing.
//
// Delete this section, the initial readiness fact, and the trust and receipt
// branches of `send_initial_prompt`/`send_terminal_follow_up` when Agy provides
// such a signal or an input API; the transcript result monitor is unaffected.

const INPUT_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(super) trait Clock {
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

pub(super) struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

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
                wait_for_workspace_trust(&directory, deadline)?;
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
