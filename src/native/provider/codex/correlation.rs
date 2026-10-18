use super::PendingCodexTurn;
use crate::native::session::{Reader, RecordReader};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::Path;

pub(super) fn valid_codex_thread_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub(super) fn correlated_prompt(prompt: &str, pending: &PendingCodexTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n{}",
        pending.marker
    )
}

pub(super) fn correlated_response<'a>(
    message: &'a str,
    pending: &PendingCodexTurn,
) -> Result<&'a str> {
    let body = message
        .trim_end()
        .strip_suffix(&pending.marker)
        .context("Codex response did not end with the expected turn marker")?
        .trim_end();
    if body.is_empty() {
        bail!("Codex correlated response contained no assistant text")
    }
    Ok(body)
}

pub(super) fn codex_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

pub(super) fn codex_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    codex_string(payload, key).map(str::to_owned)
}

// Exact IDE wrapper from Codex's prompt renderer; source evidence is in AGENTS.md, Current Codex Boundary.
const IDE_CONTEXT_HEADING: &str = "# Context from my IDE setup:";
const IDE_REQUEST_HEADING: &str = "## My request for Codex:";

// Require the delegation header, directly or after the exact IDE wrapper.
// Try every request heading because context and prompt can themselves quote it.
pub(super) fn begins_with_bridge_prompt(message: &str) -> bool {
    let header = crate::native::NATIVE_DELEGATION_HEADER;
    let message = message.trim_start();
    message.starts_with(header)
        || (message.starts_with(IDE_CONTEXT_HEADING)
            && message
                .match_indices(IDE_REQUEST_HEADING)
                .any(|(at, heading)| {
                    message[at + heading.len()..]
                        .trim_start()
                        .starts_with(header)
                }))
}

// Require header and trailing marker: Codex's title turn can quote the marker on another thread.
// Framing is not turn identity; see docs/verification/2026-10-02-windows.md.
// Any element may be the prompt: a message typed into the TUI during the turn is steered into
// that turn and follows the prompt as a later element (codex-cli 0.163.0, 2026-10-17, #65).
pub(super) fn codex_input_correlates(
    payload: &serde_json::Value,
    pending: &PendingCodexTurn,
) -> bool {
    if codex_string(payload, "type") != Some("agent-turn-complete") {
        return false;
    }
    payload
        .get("input-messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|message| {
                    begins_with_bridge_prompt(message)
                        && message.trim_end().ends_with(&pending.marker)
                })
        })
}

pub(super) fn established_codex_thread(directory: &Path) -> Result<Option<String>> {
    for path in Reader::open_unchecked(directory).events()? {
        let event: crate::native::session::schema::SessionEvent = RecordReader::at(&path).json()?;
        if event.provider == FirstPartyCli::Codex.as_str()
            && let Some(thread_id) = event.provider_session_id
        {
            return Ok(Some(thread_id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
