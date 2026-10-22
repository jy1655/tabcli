//! Claim-addressed lifecycle evidence. Bound claims settle through the transcript observer.
use super::correlation::{begins_with_bridge_prompt, valid_codex_thread_id};
use super::{codex_input_correlates, codex_string, diagnose, established_codex_thread};
use crate::native::provider::QuestionEmission;
use crate::native::session::primitives::unix_ms;
use crate::native::session::requests;
use crate::native::session::{Reader, Store, turn};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{io::Write, path::Path};

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

#[cfg(any(windows, test))]
pub(super) fn read_binding(directory: &Path, token: &str) -> Result<Option<Binding>> {
    binding(&Reader::open_unchecked(directory), token)
}

pub(super) fn binding(reader: &Reader, token: &str) -> Result<Option<Binding>> {
    let name = record_name(BINDING_PREFIX, token)?;
    let binding = reader.private(&name).optional_json::<Binding>()?;
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

pub(super) fn bind(store: &Store, payload: &Value) -> Result<()> {
    let Some(pending) = pending(store)? else {
        return Ok(());
    };
    publish_binding(store, &pending, payload)
}

// The pending snapshot is only a proposed address. The live Claim is read inside the
// lifecycle lock, immediately before comparison and publication, never before acquiring it.
fn publish_binding(store: &Store, pending: &PendingCodexTurn, payload: &Value) -> Result<()> {
    let directory = store.directory();
    let _lock = store.lock()?;
    if !current_claim(store, &pending.claim_token)? {
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
    if let Some(binding) = binding(store, &pending.claim_token)? {
        if binding.thread_id != thread_id
            || binding.turn_id != turn_id
            || binding.transcript_path != transcript_path
        {
            // Diagnostic only: never change status.error or manufacture a completion.
            store
                .private(&record_name(DIAGNOSTIC_PREFIX, &pending.claim_token)?)
                .write_json(&BindingConflict {
                    schema: 1,
                    claim_token: pending.claim_token.clone(),
                    reason: "binding_conflict".into(),
                    thread_id: thread_id.into(),
                    turn_id: turn_id.into(),
                    transcript_path: transcript_path.into(),
                    observed_unix_ms: unix_ms(),
                })?;
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
pub(super) fn current_claim(reader: &Reader, token: &str) -> Result<bool> {
    if turn::current_claim_token(reader)?.as_deref() != Some(token) {
        return Ok(false);
    }
    if !matches!(
        reader.status()?.state,
        crate::native::session::SessionState::Running
            | crate::native::session::SessionState::Working
            | crate::native::session::SessionState::ResumePending
    ) {
        return Ok(false);
    }
    let Some(current_pending) = pending(reader)? else {
        return Ok(false);
    };
    Ok(current_pending.claim_token == token)
}

pub(super) fn stop(store: &Store, payload: &Value) -> Result<()> {
    let _lock = store.lock()?;
    let Some(token) = turn::current_claim_token(store)? else {
        return Ok(());
    };
    let Some(binding) = binding(store, &token)? else {
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
    store
        .private(&record_name(CANDIDATE_PREFIX, &token)?)
        .write_json(&Candidate {
            schema: 1,
            claim_token: token,
            thread_id: binding.thread_id,
            turn_id: binding.turn_id,
            last_assistant_message: message.as_str().map(str::to_owned),
            stop_hook_active: active,
            observed_unix_ms: unix_ms(),
        })
}

// The caller may report only Unbound, after this operation has released its lock.
pub(super) enum Route {
    Bound,
    Unbound(Option<PendingCodexTurn>),
    Stale,
}

pub(super) fn notify(store: &Store, payload: &Value) -> Result<Route> {
    let Some(pending) = pending(store)? else {
        return Ok(Route::Unbound(None));
    };
    record_notify(store, pending, payload)
}
fn record_notify(store: &Store, pending: PendingCodexTurn, payload: &Value) -> Result<Route> {
    let _lock = store.lock()?;
    if turn::current_claim_token(store)?.as_deref() != Some(&pending.claim_token) {
        return Ok(Route::Stale);
    }
    let Some(binding) = binding(store, &pending.claim_token)? else {
        return Ok(Route::Unbound(Some(pending)));
    };
    // Binding is written at turn start, notify arrives at turn end: route selection
    // does not depend on a scheduling race. Bound claims are settled by the
    // transcript watcher; there is deliberately no timer or notify fallback.
    if codex_string(payload, "thread-id") == Some(&binding.thread_id)
        && codex_string(payload, "turn-id") == Some(&binding.turn_id)
    {
        store
            .private(&record_name(NOTIFY_PREFIX, &pending.claim_token)?)
            .write_json(&Notify {
                schema: 1,
                claim_token: pending.claim_token.clone(),
                thread_id: binding.thread_id,
                turn_id: binding.turn_id,
                last_assistant_message: codex_string(payload, "last-assistant-message")
                    .map(str::to_owned),
                input_messages_matched: codex_input_correlates(payload, &pending),
                observed_unix_ms: unix_ms(),
            })?;
    }
    Ok(Route::Bound)
}

#[cfg(test)]
#[path = "lifecycle/tests.rs"]
mod tests;

pub(super) const PENDING_TURN_FILE: &str = "codex-pending-turn.json";
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct PendingCodexTurn {
    pub(super) schema: u32,
    pub(super) claim_token: String,
    pub(super) marker: String,
    // The sizes of the framed prompt as Bridge handed it to delivery, kept for the doctor's
    // notify payload estimate (#127). The macOS launch argument and the queue message
    // deliver that text; the Windows console paste can drop a non-BMP character (#147),
    // so the delivered text can be smaller. Additive within schema 1: a record of 0.4.14
    // reads with `None`, and 0.4.14 ignores the fields (serde keeps unknown fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) framed_prompt_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) framed_prompt_units: Option<u64>,
}

impl PendingCodexTurn {
    pub(super) fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: marker(claim_token),
            framed_prompt_chars: None,
            framed_prompt_units: None,
        })
    }

    // Records the framed prompt's sizes before delivery: characters, and UTF-16 units of
    // its JSON escape, which is how the notify payload carries it in `input-messages`.
    pub(super) fn record_framed_prompt(&mut self, framed: &str) {
        let (chars, units) = diagnose::framed_prompt_sizes(framed);
        self.framed_prompt_chars = Some(chars);
        self.framed_prompt_units = Some(units);
    }
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if !turn::valid_turn_claim_token(claim_token) {
        bail!("invalid Codex turn claim token")
    }
    Ok(())
}

pub(super) fn write_pending_turn(directory: &Path, pending: &PendingCodexTurn) -> Result<()> {
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .write_json(pending)?;
    Ok(())
}

pub(super) fn read_pending_turn(directory: &Path) -> Result<Option<PendingCodexTurn>> {
    pending(&Reader::open_unchecked(directory))
}

pub(super) fn pending(reader: &Reader) -> Result<Option<PendingCodexTurn>> {
    let Some(text) = reader.private(PENDING_TURN_FILE).text()? else {
        return Ok(None);
    };
    let pending: PendingCodexTurn =
        serde_json::from_str(&text).context("failed to parse the pending Codex turn")?;
    let expected = PendingCodexTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Codex turn correlation state")
    }
    Ok(Some(pending))
}

pub(super) fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .remove()
}

pub(super) fn marker(claim_token: &str) -> String {
    format!("<!-- agent-bridge-codex-turn:{claim_token} -->")
}
const QUESTION_RECORD_BYTES: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
pub(super) struct QuestionRecord {
    pub(super) schema: u32,
    pub(super) request_id: String,
    pub(super) claim_token: String,
    pub(super) items: Vec<QuestionEmission>,
    pub(super) incomplete: bool,
}

#[derive(Serialize)]
pub(super) struct QuestionRecordRef<'a> {
    pub(super) schema: u32,
    pub(super) request_id: &'a str,
    pub(super) claim_token: &'a str,
    pub(super) items: &'a [QuestionEmission],
    pub(super) incomplete: bool,
}

pub(super) fn question_name(token: &str) -> String {
    format!("codex-questions-{token}.json")
}

fn valid_item(item: &QuestionEmission) -> bool {
    !item.id.is_empty()
        && !item.questions.is_empty()
        && item.questions.iter().all(|q| {
            !q.title.trim().is_empty()
                && q.options.as_ref().is_none_or(|options| {
                    !options.is_empty() && options.iter().all(|s| !s.trim().is_empty())
                })
        })
}

pub(super) fn questions(
    reader: &Reader,
    receipt: &requests::Receipt,
) -> Result<Option<QuestionRecord>> {
    let Some(bytes) = reader
        .private(&question_name(&receipt.claim_token))
        .timeline(QUESTION_RECORD_BYTES)?
    else {
        return Ok(None);
    };
    let record: QuestionRecord = serde_json::from_slice(&bytes)?;
    if record.schema != 1
        || record.request_id != receipt.request_id
        || record.claim_token != receipt.claim_token
        || !record.items.iter().all(valid_item)
        || !question_fits(&record)
        || record
            .items
            .iter()
            .enumerate()
            .any(|(i, item)| record.items[..i].iter().any(|p| p.id == item.id))
    {
        bail!("invalid Codex question record");
    }
    Ok(Some(record))
}

struct SizeLimit(u64);
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("question record limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn question_fits(value: &impl Serialize) -> bool {
    serde_json::to_writer_pretty(SizeLimit(QUESTION_RECORD_BYTES), value).is_ok()
}

const OBSERVER_PREFIX: &str = "codex-observer-";

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Candidate {
    schema: u32,
    claim_token: String,
    thread_id: String,
    turn_id: String,
    #[serde(default)]
    last_assistant_message: Option<String>,
    stop_hook_active: bool,
    observed_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Notify {
    schema: u32,
    claim_token: String,
    thread_id: String,
    turn_id: String,
    #[serde(default)]
    last_assistant_message: Option<String>,
    input_messages_matched: bool,
    observed_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct BindingConflict {
    schema: u32,
    claim_token: String,
    reason: String,
    thread_id: String,
    turn_id: String,
    transcript_path: String,
    observed_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ObserverRecord {
    pub(super) schema: u32,
    pub(super) claim_token: String,
    pub(super) state: String,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) transcript_path: Option<String>,
    pub(super) offset: u64,
    pub(super) observed_unix_ms: u128,
}

pub(super) fn write_observer(store: &Store, record: &ObserverRecord) -> Result<()> {
    store
        .private(&format!("{OBSERVER_PREFIX}{}.json", record.claim_token))
        .write_json(record)
}

// Diagnostics historically validate only the envelope (and observer state/reason).
// Retain every other field, including unknown or malformed optional evidence, exactly
// as read. Decoding a writer's full shape here would tighten that read-only contract.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Sidecar {
    schema: u32,
    claim_token: String,
    observed_unix_ms: u64,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}

fn sidecar(reader: &Reader, prefix: &str, token: &str) -> Result<Option<Sidecar>> {
    let value = reader
        .private(&format!("{prefix}{token}.json"))
        .optional_json::<Value>()?;
    let Some(value) = value else { return Ok(None) };
    let record: Sidecar = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid claim-addressed observation"))?;
    if record.schema != 1 || record.claim_token != token {
        bail!("invalid claim-addressed observation");
    }
    Ok(Some(record))
}

pub(super) fn candidate(reader: &Reader, token: &str) -> Result<Option<Sidecar>> {
    sidecar(reader, CANDIDATE_PREFIX, token)
}

// The codex-observer- observation never supplies completion authority.
pub(super) fn observer(reader: &Reader, token: &str) -> Result<Option<Sidecar>> {
    let record = sidecar(reader, OBSERVER_PREFIX, token)?;
    if let Some(record) = &record {
        let state = record.fields.get("state").and_then(Value::as_str);
        let reason = record.fields.get("reason").unwrap_or(&Value::Null);
        if !matches!(state, Some("armed" | "suspended" | "settled"))
            || !(reason.is_null() || reason.is_string())
        {
            bail!("invalid observer state");
        }
    }
    Ok(record)
}

// The operation and read-only observation have distinct namespaces: neither reader
// acquires a lifecycle lock or reads another Record.
pub(super) mod observations {
    use super::{BindingConflict, DIAGNOSTIC_PREFIX, NOTIFY_PREFIX, Notify, record_name};
    use crate::native::session::Reader;
    use anyhow::Result;

    #[allow(dead_code)] // Retained evidence has no current production consumer.
    pub(super) fn notify(reader: &Reader, token: &str) -> Result<Option<Notify>> {
        reader
            .private(&record_name(NOTIFY_PREFIX, token)?)
            .optional_json()
    }

    #[allow(dead_code)] // Conflicts are diagnostic only.
    pub(super) fn binding_conflict(
        reader: &Reader,
        token: &str,
    ) -> Result<Option<BindingConflict>> {
        reader
            .private(&record_name(DIAGNOSTIC_PREFIX, token)?)
            .optional_json()
    }
}
