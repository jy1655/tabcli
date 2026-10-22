use super::delivery::{Clock, input_receipt_window_end};
use super::log::complete_log_lines;
use super::{INPUT_RECEIPT_WINDOW, PendingAgyTurn};
use crate::native::terminal;
use anyhow::Result;
use std::time::{Duration, Instant};

const INPUT_RECEIPT_MARKER: &str = "HandleUserInput called with text: \"";
const WINDOWS_PROTOCOL_PREFIX: &str = "[Agent Bridge Agy Windows console turn protocol]";
// The framing header of `correlated_prompt`, which macOS pastes verbatim.
const TURN_PROTOCOL_HEADER: &str = "[Agent Bridge Agy turn protocol]";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InputReceipt {
    pub(super) text: String,
    pub(super) truncated: bool,
}

// Agy logs the accepted input Go-quoted (`%q`): a complete record ends with the
// closing quote. Anything else, or a trailing ellipsis, is reported as truncated for
// diagnostics; matching still requires the complete marker to be visible.
pub(super) fn parse_input_receipt(line: &str) -> Option<InputReceipt> {
    let (_, quoted) = line.split_once(INPUT_RECEIPT_MARKER)?;
    let (text, truncated) = match quoted.strip_suffix('"') {
        Some(text) => (text, false),
        None => (quoted, true),
    };
    let (text, truncated) = match text.strip_suffix("...") {
        Some(text) => (text, true),
        None => (text, truncated),
    };
    Some(InputReceipt {
        text: text.to_owned(),
        truncated,
    })
}

pub(super) fn input_receipts(log: &[u8]) -> Vec<InputReceipt> {
    complete_log_lines(log)
        .filter_map(|line| parse_input_receipt(&line))
        .collect()
}

// The complete marker (`<!-- agent-bridge-agy-turn:<token> -->`) is unique to this
// submission; a visible prefix is not, because another turn's token can share it. A
// receipt that Agy cut before the closing ` -->` therefore never confirms delivery.
// The framing header proves the record is the adapter's own paste rather than a line
// typed into the composer that happens to quote the marker; either adapter-owned
// framing (the Windows JSON envelope, the macOS verbatim prompt) qualifies.
pub(super) fn receipt_matches(receipt: &InputReceipt, pending: &PendingAgyTurn) -> bool {
    (receipt.text.contains(WINDOWS_PROTOCOL_PREFIX) || receipt.text.contains(TURN_PROTOCOL_HEADER))
        && receipt.text.contains(&pending.marker)
}

// Byte offset of the first line that begins at or after the pre-paste offset. A line
// that straddles the offset started before the paste and is never evidence.
fn evidence_start(log: &[u8], pre_paste_len: usize) -> Option<usize> {
    if pre_paste_len == 0 || log[..pre_paste_len].ends_with(b"\n") {
        return Some(pre_paste_len);
    }
    log[pre_paste_len..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|newline| pre_paste_len + newline + 1)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReceiptEvidence {
    Delivered,
    LogMissing,
    LogShrunk { len: usize },
    NoReceipt { appended: usize, partial_tail: bool },
}

pub(super) fn observe_input_receipt(
    log: Option<&[u8]>,
    pre_paste_len: usize,
    pending: &PendingAgyTurn,
) -> ReceiptEvidence {
    let Some(log) = log else {
        return ReceiptEvidence::LogMissing;
    };
    if log.len() < pre_paste_len {
        return ReceiptEvidence::LogShrunk { len: log.len() };
    }
    if let Some(start) = evidence_start(log, pre_paste_len)
        && complete_log_lines(&log[start..])
            .filter_map(|line| parse_input_receipt(&line))
            .any(|receipt| receipt_matches(&receipt, pending))
    {
        return ReceiptEvidence::Delivered;
    }
    ReceiptEvidence::NoReceipt {
        appended: log.len() - pre_paste_len,
        partial_tail: log.last().is_some_and(|byte| *byte != b'\n'),
    }
}

fn unconfirmed_receipt_error(
    evidence: &ReceiptEvidence,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    elapsed: Duration,
    full_window: bool,
) -> anyhow::Error {
    let reason = match evidence {
        ReceiptEvidence::Delivered => unreachable!("a delivered receipt is not unconfirmed"),
        ReceiptEvidence::LogMissing => "agy.log is missing after the paste".to_owned(),
        ReceiptEvidence::LogShrunk { len } => format!(
            "agy.log shrank to {len} bytes below the pre-paste offset {pre_paste_len} (rotated or truncated), so the receipt may have been lost"
        ),
        ReceiptEvidence::NoReceipt {
            appended,
            partial_tail,
        } => {
            let observed = format!(
                "no HandleUserInput receipt in the {appended} bytes appended after the pre-paste offset {pre_paste_len}"
            );
            if !full_window {
                format!(
                    "{observed}; the deadline ended the receipt window after {} of the {} second window",
                    elapsed.as_secs(),
                    INPUT_RECEIPT_WINDOW.as_secs()
                )
            } else if *partial_tail {
                format!(
                    "{observed}; agy.log ends with a partial line that may still become the receipt"
                )
            } else {
                format!(
                    "{observed} within {} seconds; Agy logs no marker that proves the console input was drained without a receipt, so non-delivery cannot be proven",
                    elapsed.as_secs()
                )
            }
        }
    };
    anyhow::anyhow!(
        "Agy input receipt for turn marker {} was not confirmed: {reason}; the console paste may have been accepted and is not repeated",
        pending.marker
    )
}

pub(super) fn confirm_input_receipt_with<L, C>(
    read_log: &mut L,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    pasted_at: Instant,
    deadline: Instant,
    poll_interval: Duration,
    clock: &mut C,
) -> terminal::TerminalSendResult
where
    L: FnMut() -> Result<Option<Vec<u8>>>,
    C: Clock,
{
    use terminal::TerminalSendFailure;
    let window_end = input_receipt_window_end(pasted_at, deadline);
    let full_window = window_end.saturating_duration_since(pasted_at) >= INPUT_RECEIPT_WINDOW;
    loop {
        let log = match read_log() {
            Ok(log) => log,
            Err(error) => {
                return Err(TerminalSendFailure::delivery_uncertain(error.context(
                    format!(
                        "Agy input receipt for turn marker {} could not be verified because agy.log is unreadable; the console paste may have been accepted and is not repeated",
                        pending.marker
                    ),
                )));
            }
        };
        let evidence = observe_input_receipt(log.as_deref(), pre_paste_len, pending);
        let now = clock.now();
        let elapsed = now.saturating_duration_since(pasted_at);
        match evidence {
            ReceiptEvidence::Delivered => return Ok(()),
            ReceiptEvidence::LogShrunk { .. } => {
                return Err(TerminalSendFailure::delivery_uncertain(
                    unconfirmed_receipt_error(
                        &evidence,
                        pending,
                        pre_paste_len,
                        elapsed,
                        full_window,
                    ),
                ));
            }
            ReceiptEvidence::LogMissing | ReceiptEvidence::NoReceipt { .. } => {}
        }
        if now >= window_end {
            return Err(TerminalSendFailure::delivery_uncertain(
                unconfirmed_receipt_error(&evidence, pending, pre_paste_len, elapsed, full_window),
            ));
        }
        clock.sleep(window_end.saturating_duration_since(now).min(poll_interval));
    }
}

#[cfg(test)]
mod tests;
