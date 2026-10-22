//! Claim-addressed lifecycle evidence. Bound claims settle through the transcript observer.
use super::correlation::{begins_with_bridge_prompt, valid_codex_thread_id};
use super::{
    PendingCodexTurn, codex_input_correlates, codex_string, established_codex_thread,
    read_pending_turn,
};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{Reader, Store, turn};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const BINDING_PREFIX: &str = "codex-turn-";
const CANDIDATE_PREFIX: &str = "codex-candidate-";
const NOTIFY_PREFIX: &str = "codex-notify-";
const DIAGNOSTIC_PREFIX: &str = "codex-binding-conflict-";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) schema: u32,
    pub(super) claim_token: String,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) transcript_path: String,
    pub(super) bound_unix_ms: u128,
}

fn record_name(prefix: &str, token: &str) -> Result<String> {
    if !turn::valid_turn_claim_token(token) {
        bail!("invalid Codex lifecycle claim token");
    }
    Ok(format!("{prefix}{token}.json"))
}

pub(super) fn read_binding(directory: &Path, token: &str) -> Result<Option<Binding>> {
    let name = record_name(BINDING_PREFIX, token)?;
    let binding = Reader::open_unchecked(directory)
        .private(&name)
        .optional_json::<Binding>()?;
    if let Some(binding) = &binding
        && (binding.schema != 1
            || binding.claim_token != token
            || !valid_codex_thread_id(&binding.thread_id)
            || binding.turn_id.is_empty())
    {
        bail!("invalid Codex lifecycle binding");
    }
    Ok(binding)
}

pub(super) fn bind(directory: &Path, payload: &Value) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    publish_binding(directory, &pending, payload)
}

// The pending snapshot is only a proposed address. The live Claim is read inside the
// lifecycle lock, immediately before comparison and publication, never before acquiring it.
fn publish_binding(directory: &Path, pending: &PendingCodexTurn, payload: &Value) -> Result<()> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    if !current_claim(directory, &pending.claim_token)? {
        return Ok(());
    }
    let (Some(prompt), Some(thread_id), Some(turn_id), Some(transcript_path)) = (
        codex_string(payload, "prompt"),
        codex_string(payload, "session_id"),
        codex_string(payload, "turn_id"),
        codex_string(payload, "transcript_path"),
    ) else {
        return Ok(());
    };
    if !begins_with_bridge_prompt(prompt)
        || !prompt.trim_end().ends_with(&pending.marker)
        || !valid_codex_thread_id(thread_id)
        || turn_id.is_empty()
        || established_codex_thread(directory)?
            .as_deref()
            .is_some_and(|id| id != thread_id)
    {
        return Ok(());
    }
    let name = record_name(BINDING_PREFIX, &pending.claim_token)?;
    if let Some(binding) = read_binding(directory, &pending.claim_token)? {
        if binding.thread_id != thread_id
            || binding.turn_id != turn_id
            || binding.transcript_path != transcript_path
        {
            // Diagnostic only: never change status.error or manufacture a completion.
            store
                .private(&record_name(DIAGNOSTIC_PREFIX, &pending.claim_token)?)
                .write_json(&json!({
                    "schema": 1, "claim_token": pending.claim_token, "reason": "binding_conflict",
                    "thread_id": thread_id, "turn_id": turn_id, "transcript_path": transcript_path,
                    "observed_unix_ms": unix_ms()
                }))?;
        }
        return Ok(());
    }
    store.private(&name).write_json(&Binding {
        schema: 1,
        claim_token: pending.claim_token.clone(),
        thread_id: thread_id.into(),
        turn_id: turn_id.into(),
        transcript_path: transcript_path.into(),
        bound_unix_ms: unix_ms(),
    })
}

// Caller holds the lifecycle lock.
pub(super) fn current_claim(directory: &Path, token: &str) -> Result<bool> {
    if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref() != Some(token) {
        return Ok(false);
    }
    if !matches!(
        Reader::open_unchecked(directory).status()?.state,
        crate::native::session::SessionState::Running
            | crate::native::session::SessionState::Working
            | crate::native::session::SessionState::ResumePending
    ) {
        return Ok(false);
    }
    let Some(current_pending) = read_pending_turn(directory)? else {
        return Ok(false);
    };
    Ok(current_pending.claim_token == token)
}

pub(super) fn stop(directory: &Path, payload: &Value) -> Result<()> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    let Some(token) = turn::current_claim_token(&Reader::open_unchecked(directory))? else {
        return Ok(());
    };
    let Some(binding) = read_binding(directory, &token)? else {
        return Ok(());
    };
    if codex_string(payload, "session_id") != Some(&binding.thread_id)
        || codex_string(payload, "turn_id") != Some(&binding.turn_id)
    {
        return Ok(());
    }
    let Some(active) = payload.get("stop_hook_active").and_then(Value::as_bool) else {
        return Ok(());
    };
    let Some(message) = payload
        .get("last_assistant_message")
        .filter(|v| v.is_null() || v.is_string())
    else {
        return Ok(());
    };
    store.private(&record_name(CANDIDATE_PREFIX, &token)?).write_json(&json!({
        "schema": 1, "claim_token": token, "thread_id": binding.thread_id, "turn_id": binding.turn_id,
        "last_assistant_message": message, "stop_hook_active": active, "observed_unix_ms": unix_ms()
    }))
}

// True means no Report call is allowed (bound or stale). Drop this lock before Report.
pub(super) fn record_notify(
    directory: &Path,
    pending: &PendingCodexTurn,
    payload: &Value,
) -> Result<bool> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
        != Some(&pending.claim_token)
    {
        return Ok(true);
    }
    let Some(binding) = read_binding(directory, &pending.claim_token)? else {
        return Ok(false);
    };
    // Binding is written at turn start, notify arrives at turn end: route selection
    // does not depend on a scheduling race. Bound claims are settled by the
    // transcript watcher; there is deliberately no timer or notify fallback.
    if codex_string(payload, "thread-id") == Some(&binding.thread_id)
        && codex_string(payload, "turn-id") == Some(&binding.turn_id)
    {
        store.private(&record_name(NOTIFY_PREFIX, &pending.claim_token)?).write_json(&json!({
            "schema": 1, "claim_token": pending.claim_token, "thread_id": binding.thread_id,
            "turn_id": binding.turn_id, "last_assistant_message": codex_string(payload, "last-assistant-message"),
            "input_messages_matched": codex_input_correlates(payload, pending), "observed_unix_ms": unix_ms()
        }))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
