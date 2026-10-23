use super::turn::{self as claude_turn, ClaudeTurn, Installed};
use super::valid_claude_conversation_id;
use crate::native::clock::Clock;
use crate::native::session::{Reader, Store, primitives::unix_ms, turn};
use agent_bridge::{FirstPartyCli, terminal_safe_text};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

const MODE_FILE: &str = "claude-completion-mode.json";
const READY_FILE: &str = "claude-mod-ready.json";
const DIAGNOSTIC_FILE: &str = "claude-mod-diagnostic.json";
const REVISION: u32 = 1;
const MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FROM_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 256;
const MAX_MODE_BYTES: usize = 32;
const MAX_TURN_BYTES: usize = 128;
const MODULE: &str = include_str!("completion.js");

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    NativeV1,
    LegacyStop,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct LaunchMode {
    schema: u32,
    mode: Mode,
    reason: String,
    claude_version: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Ready {
    schema: u32,
    revision: u32,
    conversation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Binding {
    schema: u32,
    claim: String,
    correlation_id: String,
    conversation: String,
    turn_id: String,
}

#[derive(Deserialize, Serialize)]
struct Candidate {
    schema: u32,
    binding: Binding,
    answer_preview: String,
    created_unix_ms: u128,
}

impl Candidate {
    fn matches(&self, binding: &Binding) -> bool {
        self.schema == 1
            && self.binding == *binding
            && self.created_unix_ms > 0
            && self.answer_preview.chars().count() <= 1024
    }
}

#[derive(Deserialize, Serialize)]
struct Continuation {
    schema: u32,
    binding: Binding,
    turn_id: String,
    created_unix_ms: u128,
}

enum Outcome {
    Answer { text: String, marked: bool },
    Failed(String),
}

pub(super) fn native_mode(directory: &Path) -> Result<bool> {
    match Reader::open_unchecked(directory)
        .private(MODE_FILE)
        .optional_json::<LaunchMode>()?
    {
        Some(mode) if mode.schema == 1 => Ok(mode.mode == Mode::NativeV1),
        Some(_) => bail!("unsupported Claude completion mode schema"),
        None => Ok(false),
    }
}

pub(super) fn prepare(directory: &Path, windows: bool) -> Result<Vec<OsString>> {
    let version = Reader::open_unchecked(directory)
        .manifest()
        .context("Claude completion requires the recorded launch version")?
        .provider_version;
    let parsed = version
        .split_whitespace()
        .find_map(|word| semver::Version::parse(word).ok());
    if !parsed
        .is_some_and(|version| version.pre.is_empty() && version >= semver::Version::new(2, 1, 302))
    {
        let platform = if windows { "native Windows" } else { "macOS" };
        bail!(
            "{platform} Claude completion requires stable Claude Code 2.1.302 or newer; recorded version: {version}"
        );
    }
    let mode = LaunchMode {
        schema: 1,
        mode: Mode::NativeV1,
        reason: if windows {
            "windows_stable_2_1_302_or_newer"
        } else {
            "macos_stable_2_1_302_or_newer"
        }
        .into(),
        claude_version: Some(version),
    };
    let store = Store::open_unchecked(directory);
    if let Some(existing) = store.private(MODE_FILE).optional_json::<LaunchMode>()? {
        if existing != mode {
            bail!("a different Claude completion mode was already recorded for this launch");
        }
    } else {
        store.private(MODE_FILE).write_json(&mode)?;
    }
    let plugin = directory.join("claude-mod");
    std::fs::create_dir_all(plugin.join(".claude-plugin"))?;
    std::fs::create_dir_all(plugin.join("hooks"))?;
    store.private("claude-mod/.claude-plugin/plugin.json").write_json(&json!({
        "name":"agent-bridge-completion", "version":"1.0.0", "description":"Observe official Claude turn completion for Agent Bridge."
    }))?;
    store
        .private("claude-mod/hooks/hooks.json")
        .write_json(&json!({"modules":["./register.js"]}))?;
    store
        .private("claude-mod/hooks/register.js")
        .write_private(MODULE.as_bytes())?;
    Ok(vec!["--plugin-dir".into(), plugin.into_os_string()])
}

pub(super) fn settings(_executable: &Path) -> Value {
    json!({"crossSessionInbound":"accept"})
}

pub(super) fn admit(
    directory: &Path,
    token: &str,
    deadline: Instant,
    windows: bool,
    clock: &mut impl Clock,
) -> Result<()> {
    if !native_mode(directory)? {
        if windows {
            bail!(
                "Claude session was launched before native per-turn completion; close and reopen it before sending a new request"
            );
        }
        bail!(
            "Claude session was launched before native per-turn completion; inspect its result and terminal, close it when appropriate, then start a fresh ask with the needed context; no message was sent"
        );
    }
    loop {
        claude_turn::require_current(&Reader::open_unchecked(directory), token)?;
        let remaining = deadline.checked_duration_since(clock.now()).filter(|remaining| !remaining.is_zero())
            .context("Claude completion module readiness was not observed before the delivery deadline; inspect module load, plugin trust and provider policy; no message was sent")?;
        if let Some(ready) = Reader::open_unchecked(directory)
            .private(READY_FILE)
            .optional_json::<Ready>()?
        {
            if ready.schema != 1
                || ready.revision != REVISION
                || !valid_claude_conversation_id(&ready.conversation)
            {
                bail!("Claude completion module readiness record is invalid; no message was sent");
            }
            return Ok(());
        }
        clock.sleep(remaining.min(Duration::from_millis(50)));
    }
}

pub(super) fn install_pending(
    directory: &Path,
    request_id: &str,
    token: Option<&str>,
    expected_message: &str,
) -> Result<Installed> {
    let pending = ClaudeTurn::new(request_id)?;
    let store = Store::open_unchecked(directory);
    if !native_mode(directory)? {
        return pending.install(&store);
    }
    let token = token.context("Claude native completion requires the delivery claim")?;
    if start_wire_size(expected_message)? > MAX_WIRE_BYTES {
        bail!(
            "Claude request exceeds the 4 MiB native completion wire budget; no message was sent"
        );
    }
    pending.install_native(&store, token, expected_message)
}

fn start_wire_size(expected: &str) -> Result<usize> {
    let header = format!(
        "<cross-session-message from=\"{}\" from-name=\"{}\" from-mode=\"{}\">",
        "\\".repeat(MAX_FROM_BYTES),
        "\\".repeat(MAX_NAME_BYTES),
        "x".repeat(MAX_MODE_BYTES)
    );
    let mut quoted = String::with_capacity(expected.len());
    let mut characters = expected.chars().peekable();
    while let Some(character) = characters.next() {
        quoted.push(character);
        if character == '<' && characters.peek() != Some(&'\\') {
            quoted.push('\\');
        }
    }
    Ok(serde_json::to_vec(&json!({"agent_bridge_claude_mod":1,"event":"turn.start","conversation":"00000000-0000-0000-0000-000000000000","data":{"turnId":"x".repeat(MAX_TURN_BYTES),"text":format!("{header}\n{quoted}\n</cross-session-message>")}}))?.len())
}

fn exact_frame(text: &str, expected: &str) -> bool {
    let Some((header, body)) = text
        .strip_suffix("\n</cross-session-message>")
        .and_then(|text| text.split_once('\n'))
    else {
        return false;
    };
    let Some(attributes) = header
        .strip_prefix("<cross-session-message from=\"")
        .and_then(|header| header.strip_suffix("\">"))
    else {
        return false;
    };
    let Some((from, attributes)) = attributes.split_once("\" from-name=\"") else {
        return false;
    };
    let Some((name, mode)) = attributes.split_once("\" from-mode=\"") else {
        return false;
    };
    let attribute = |value: &str, limit| {
        !value.is_empty()
            && value.len() <= limit
            && value
                .chars()
                .all(|c| !c.is_control() && !matches!(c, '"' | '<' | '>'))
    };
    attribute(from, MAX_FROM_BYTES)
        && attribute(name, MAX_NAME_BYTES)
        && !mode.is_empty()
        && mode.len() <= MAX_MODE_BYTES
        && mode
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        && matches_peer_quoted_body(body, expected)
}

fn matches_peer_quoted_body(body: &str, expected: &str) -> bool {
    // Claude 2.1.302 quotes protected peer-body tags by inserting a backslash after '<'.
    let mut body = body.chars().peekable();
    let mut expected = expected.chars().peekable();
    while let Some(character) = expected.next() {
        if body.next() != Some(character) {
            return false;
        }
        if character == '<' && expected.peek() != Some(&'\\') && body.peek() == Some(&'\\') {
            body.next();
        }
    }
    body.next().is_none()
}

fn diagnostic(directory: &Path, event: &str, reason: &str) -> Result<()> {
    Store::open_unchecked(directory)
        .private(DIAGNOSTIC_FILE)
        .write_json(&json!({
            "schema":1,"event":bounded(event),"reason":bounded(reason),
        }))
}

fn bounded(text: &str) -> String {
    terminal_safe_text(text, true).chars().take(1024).collect()
}

fn identity<'a>(data: &'a Value, key: &str) -> Result<&'a str> {
    let value = data
        .get(key)
        .and_then(Value::as_str)
        .context("Claude module identity field is missing or not text")?;
    if !valid_identity(value) {
        bail!("Claude module identity field is invalid");
    }
    Ok(value)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TURN_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn candidate_name(token: &str) -> String {
    format!("claude-mod-candidate-{token}.json")
}

fn remember_candidate(store: &Store, binding: &Binding, answer: &str) -> Result<()> {
    let record = store.private(&candidate_name(&binding.claim));
    match record.optional_json::<Candidate>()? {
        Some(candidate) if candidate.matches(binding) => Ok(()),
        Some(_) => bail!("Claude completion candidate does not match the immutable binding"),
        None => record.write_json(&Candidate {
            schema: 1,
            binding: binding.clone(),
            answer_preview: bounded(answer),
            created_unix_ms: unix_ms(),
        }),
    }
}

// Called under the lifecycle lock, after the live claim, pending request, original
// binding, conversation and fully decoded marked answer have all been verified.
fn select_continuation(
    store: &Store,
    binding: &Binding,
    turn_id: &str,
) -> Result<Option<Continuation>> {
    let candidate: Candidate = store
        .private(&candidate_name(&binding.claim))
        .optional_json()?
        .context("Claude continuation has no unmarked bound-turn candidate")?;
    if !candidate.matches(binding) {
        bail!("Claude continuation candidate does not match the immutable binding");
    }
    let record = store.private(&format!("claude-mod-continuation-{}.json", binding.claim));
    if let Some(selected) = record.optional_json::<Continuation>()? {
        if selected.schema != 1
            || selected.binding != *binding
            || selected.turn_id != turn_id
            || selected.created_unix_ms == 0
        {
            bail!("Claude continuation conflicts with the already selected completion identity");
        }
        return Ok(None);
    }
    // Preserve both official identities before publication. A helper retry can repeat
    // this selection, but another turn cannot replace it if publication was interrupted.
    Ok(Some(Continuation {
        schema: 1,
        binding: binding.clone(),
        turn_id: turn_id.into(),
        created_unix_ms: unix_ms(),
    }))
}

fn binding_matches_request(binding: &Binding, pending: &ClaudeTurn, token: &str) -> bool {
    binding.schema == 1
        && binding.claim == token
        && binding.correlation_id == pending.id()
        && valid_claude_conversation_id(&binding.conversation)
        && valid_identity(&binding.turn_id)
}

/// Called only under inspect's existing lifecycle reader lock. This observes private
/// evidence; it neither selects a completion nor repairs or publishes any result.
pub(super) fn observe_candidate(reader: &Reader, token: &str) -> Option<Value> {
    if !turn::valid_turn_claim_token(token) {
        return None;
    }
    let read = || -> Result<Option<Value>> {
        if !native_mode(reader.directory())? {
            return Ok(None);
        }
        let Some(candidate) = reader
            .private(&candidate_name(token))
            .optional_json::<Candidate>()?
        else {
            return Ok(None);
        };
        let pending = claude_turn::read_native(reader, token)?;
        let binding: Binding = reader
            .private(&format!("claude-mod-bound-{token}.json"))
            .json()?;
        if !binding_matches_request(&binding, &pending, token) || !candidate.matches(&binding) {
            bail!("Claude candidate does not match the current request and original binding");
        }
        let selected = reader
            .private(&format!("claude-mod-continuation-{token}.json"))
            .optional_json::<Continuation>()?;
        if let Some(selected) = &selected
            && (selected.schema != 1
                || selected.binding != binding
                || !valid_identity(&selected.turn_id)
                || selected.turn_id == binding.turn_id
                || selected.created_unix_ms == 0)
        {
            bail!("Claude selected continuation identity is invalid");
        }
        Ok(Some(json!({
            "state":"unmarked_bound_turn",
            "detail":"The bound Claude turn ended without the final marker. This candidate is not a published result. A caller timeout does not release the claim; inspect the provider surface before closing or diagnosing the session.",
            "correlation_id":binding.correlation_id,
            "conversation":binding.conversation,
            "bound_turn_id":binding.turn_id,
            "answer_preview":candidate.answer_preview,
            "created_unix_ms":candidate.created_unix_ms,
            "selected_completion_turn_id":selected.map(|selected| selected.turn_id),
        })))
    };
    match read() {
        Ok(observation) => observation,
        Err(error) => Some(json!({"state":"unreadable","detail":bounded(&format!("{error:#}"))})),
    }
}

pub(super) fn receive(directory: &Path, wire: &Value) -> Result<()> {
    if !native_mode(directory)? {
        return Ok(());
    }
    let event = wire
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let decoded = || -> Result<(&str, &Value)> {
        if wire.get("agent_bridge_claude_mod").and_then(Value::as_u64) != Some(1) {
            bail!("unsupported Claude module wire schema");
        }
        if serde_json::to_vec(wire)?.len() > MAX_WIRE_BYTES {
            bail!("Claude module wire exceeds 4 MiB; no outcome accepted");
        }
        let conversation = wire
            .get("conversation")
            .and_then(Value::as_str)
            .filter(|value| valid_claude_conversation_id(value))
            .context("invalid Claude module conversation")?;
        let data = wire
            .get("data")
            .filter(|value| value.is_object())
            .context("invalid Claude module event data")?;
        Ok((conversation, data))
    };
    let (conversation, data) = match decoded() {
        Ok(decoded) => decoded,
        Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
    };
    if data.get("agentId").is_some() {
        return Ok(());
    }
    let store = Store::open_unchecked(directory);
    let lock = store.lock()?;
    if store.closed_if_present()?.is_some() || store.status()?.state.ended() {
        return Ok(());
    }
    if event == "session.start" {
        return store.private(READY_FILE).write_json(&Ready {
            schema: 1,
            revision: REVISION,
            conversation: conversation.into(),
        });
    }
    let Some(token) = turn::current_claim_token(&store)? else {
        return Ok(());
    };
    if let Err(error) = claude_turn::require_current(&store, &token) {
        return diagnostic(directory, event, &format!("{error:#}"));
    }
    let pending = match claude_turn::read_native(&store, &token) {
        Ok(pending) => pending,
        Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
    };
    let turn_id = match identity(data, "turnId") {
        Ok(id) => id,
        Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
    };
    let binding_path = format!("claude-mod-bound-{token}.json");
    let proposed = Binding {
        schema: 1,
        claim: token.clone(),
        correlation_id: pending.id().to_owned(),
        conversation: conversation.into(),
        turn_id: turn_id.into(),
    };
    let binding: Option<Binding> = match store.private(&binding_path).optional_json() {
        Ok(binding) => binding,
        Err(error) => {
            return diagnostic(
                directory,
                event,
                &format!("invalid Claude turn binding: {error:#}"),
            );
        }
    };
    if event == "turn.start" {
        if !data
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| {
                exact_frame(text, pending.expected_message().expect("validated pending"))
            })
        {
            return diagnostic(
                directory,
                event,
                "Claude turn.start did not match the pending request's entire cross-session frame",
            );
        }
        return match binding {
            None => store.private(&binding_path).write_json(&proposed),
            Some(binding) if binding == proposed => Ok(()),
            Some(_) => diagnostic(
                directory,
                event,
                "conflicting Claude turn identity; the original binding was retained",
            ),
        };
    }
    let Some(binding) = binding.filter(|binding| {
        binding_matches_request(binding, &pending, &token) && binding.conversation == conversation
    }) else {
        return diagnostic(
            directory,
            event,
            "Claude module event has no matching turn.start binding for the pending request; unrelated turns and input without its own exact start cannot settle it",
        );
    };
    let bound = binding == proposed;
    if !bound && event != "turn.complete" {
        return diagnostic(
            directory,
            event,
            "unbound Claude event has no marked completion evidence",
        );
    }
    let outcome = if event == "oversize" {
        match data
            .get("utf8_len")
            .and_then(Value::as_u64)
            .filter(|size| *size > MAX_WIRE_BYTES as u64)
        {
            Some(size) => Ok(Outcome::Failed(format!(
                "Claude turn.complete wire exceeded the 4 MiB limit ({size} UTF-8 bytes); the answer was not truncated"
            ))),
            None => Err(anyhow::anyhow!(
                "invalid oversized Claude completion length"
            )),
        }
    } else if event == "turn.complete" {
        outcome(data, pending.marker())
    } else {
        Err(anyhow::anyhow!("unsupported Claude module event"))
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
    };
    let (outcome, provenance) = if bound {
        if let Outcome::Answer {
            text,
            marked: false,
        } = &outcome
        {
            remember_candidate(&store, &binding, text)?;
            return diagnostic(
                directory,
                event,
                "the bound Claude turn ended without the final marker; waiting for a marked completion in the same conversation; a caller timeout does not release the claim; inspect the session before closing or diagnosing it",
            );
        }
        (outcome, "claude_mod_turn_complete")
    } else {
        if !matches!(&outcome, Outcome::Answer { marked: true, .. }) {
            return diagnostic(
                directory,
                event,
                "unbound Claude completion is not a valid nonempty answer with the exact pending marker; the request remains pending",
            );
        }
        let selection = match select_continuation(&store, &binding, turn_id) {
            Ok(selection) => selection,
            Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
        };
        if let Some(selection) = selection {
            store
                .private(&format!("claude-mod-continuation-{token}.json"))
                .write_json(&selection)?;
        }
        (outcome, "claude_mod_marked_continuation")
    };
    drop(lock);
    let report = turn::Report::for_claim(&store, FirstPartyCli::Claude, Some(&token))
        .with_provenance(Some(provenance));
    match outcome {
        Outcome::Answer { text, .. } => report.complete(
            &text,
            Some(conversation.into()),
            Some(pending.id().to_owned()),
        ),
        Outcome::Failed(error) => report.fail(
            &error,
            Some(conversation.into()),
            Some(pending.id().to_owned()),
        ),
    }
}

fn outcome(data: &Value, marker: &str) -> Result<Outcome> {
    let reason = data
        .get("reason")
        .and_then(Value::as_str)
        .context("Claude completion reason is missing")?;
    let aborted = data
        .get("isAborted")
        .and_then(Value::as_bool)
        .context("Claude completion interrupt flag is missing")?;
    let answer = data
        .get("answer")
        .and_then(Value::as_str)
        .context("Claude completion answer is missing or not text")?;
    if aborted != (reason == "aborted") || (reason != "refusal" && data.get("refusal").is_some()) {
        bail!("inconsistent Claude completion outcome fields");
    }
    Ok(match reason {
        "answer" => {
            let answer = answer.trim_end();
            let marked = answer.ends_with(marker);
            let answer = answer.strip_suffix(marker).unwrap_or(answer).trim_end();
            if answer.trim().is_empty() {
                Outcome::Failed("Claude ended the turn with no answer text".into())
            } else {
                Outcome::Answer {
                    text: answer.into(),
                    marked,
                }
            }
        }
        "aborted" => Outcome::Failed("Claude turn was interrupted".into()),
        "error" => Outcome::Failed(if answer.trim().is_empty() {
            "Claude turn ended on an API error".into()
        } else {
            format!("Claude turn ended on an API error: {}", bounded(answer))
        }),
        "refusal" => {
            let refusal = data
                .get("refusal")
                .and_then(Value::as_object)
                .context("Claude refusal detail is missing")?;
            let mut details = vec![];
            for key in ["category", "explanation"] {
                match refusal.get(key) {
                    Some(Value::Null) => {}
                    Some(Value::String(text)) => {
                        if !text.trim().is_empty() {
                            details.push(bounded(text));
                        }
                    }
                    _ => bail!("Claude refusal field is missing or malformed"),
                }
            }
            Outcome::Failed(if details.is_empty() {
                "Claude refused the turn".into()
            } else {
                format!("Claude refused the turn: {}", details.join(": "))
            })
        }
        _ => bail!("unknown Claude completion reason"),
    })
}

#[cfg(test)]
mod tests;
