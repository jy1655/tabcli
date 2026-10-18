use super::{
    PENDING_TURN_FILE, PendingCrossSessionTurn, PendingTurnFile, valid_claude_conversation_id,
};
use crate::native::clock::Clock;
use crate::native::session::{Reader, Store, turn};
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

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct Binding {
    schema: u32,
    claim: String,
    correlation_id: String,
    conversation: String,
    turn_id: String,
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
    let version = match Reader::open_unchecked(directory).manifest() {
        Ok(manifest) => Some(manifest.provider_version),
        Err(error)
            if !windows
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => {
            return Err(error).context("Claude completion requires the recorded launch version");
        }
    };
    if windows {
        let parsed = version.as_deref().and_then(|value| {
            value
                .split_whitespace()
                .find_map(|word| semver::Version::parse(word).ok())
        });
        if !parsed.is_some_and(|version| {
            version.pre.is_empty() && version >= semver::Version::new(2, 1, 302)
        }) {
            bail!(
                "native Windows Claude completion requires stable Claude Code 2.1.302 or newer; recorded version: {}",
                version.as_deref().unwrap_or("unknown")
            );
        }
    }
    let mode = LaunchMode {
        schema: 1,
        mode: if windows {
            Mode::NativeV1
        } else {
            Mode::LegacyStop
        },
        reason: if windows {
            "windows_verified_profile"
        } else {
            "platform_legacy"
        }
        .into(),
        claude_version: version,
    };
    let store = Store::open_unchecked(directory);
    if let Some(existing) = store.private(MODE_FILE).optional_json::<LaunchMode>()? {
        if existing != mode {
            bail!("Claude completion mode was already selected for this launch");
        }
    } else {
        store.private(MODE_FILE).write_json(&mode)?;
    }
    if !windows {
        return Ok(vec![]);
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

pub(super) fn settings(executable: &Path, windows: bool) -> Value {
    if windows {
        json!({"crossSessionInbound":"accept"})
    } else {
        super::hook_settings(executable)
    }
}

fn require_current(directory: &Path, token: &str) -> Result<()> {
    let reader = Reader::open_unchecked(directory);
    if !turn::valid_turn_claim_token(token)
        || turn::current_claim_token(&reader)?.as_deref() != Some(token)
        || reader.closed_if_present()?.is_some()
        || !matches!(
            reader.status()?.state,
            crate::native::session::SessionState::Working
                | crate::native::session::SessionState::Running
                | crate::native::session::SessionState::ResumePending
        )
    {
        bail!("Claude completion delivery refused: this claim or session is no longer current");
    }
    Ok(())
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
        return Ok(());
    }
    loop {
        require_current(directory, token)?;
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
) -> Result<PendingTurnFile> {
    if !native_mode(directory)? {
        return super::install_pending_turn(directory, request_id);
    }
    let token = token.context("Claude native completion requires the delivery claim")?;
    let mut pending = PendingCrossSessionTurn::new(request_id)?;
    if start_wire_size(expected_message)? > MAX_WIRE_BYTES {
        bail!(
            "Claude request exceeds the 4 MiB native completion wire budget; no message was sent"
        );
    }
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    require_current(directory, token)?;
    pending.schema = 2;
    pending.claim_token = Some(token.into());
    pending.expected_message = Some(expected_message.into());
    store.private(PENDING_TURN_FILE).write_json(&pending)?;
    Ok(PendingTurnFile {
        path: directory.join(PENDING_TURN_FILE),
        request_id: request_id.into(),
        retained: false,
    })
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
    if value.is_empty()
        || value.len() > MAX_TURN_BYTES
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("Claude module identity field is invalid");
    }
    Ok(value)
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
    if let Err(error) = require_current(directory, &token) {
        return diagnostic(directory, event, &format!("{error:#}"));
    }
    let parse_pending = || -> Result<PendingCrossSessionTurn> {
        let pending: PendingCrossSessionTurn = store.private(PENDING_TURN_FILE).json()?;
        let expected = PendingCrossSessionTurn::new(&pending.request_id)?;
        if pending.schema != 2
            || pending.marker != expected.marker
            || pending.claim_token.as_deref() != Some(&token)
            || !pending.expected_message.as_deref().is_some_and(|text| {
                text.starts_with("[Agent Bridge native delegation]\nSource: ")
                    && text.ends_with(&pending.marker)
            })
        {
            bail!("Claude native pending record does not identify the current framed request");
        }
        Ok(pending)
    };
    let pending = match parse_pending() {
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
        correlation_id: pending.request_id.clone(),
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
                exact_frame(
                    text,
                    pending
                        .expected_message
                        .as_deref()
                        .expect("validated pending"),
                )
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
    if binding.as_ref() != Some(&proposed) {
        return diagnostic(
            directory,
            event,
            "No matching Claude turn.start binding was observed for this pending request; input delivered into an already-running turn cannot be correlated",
        );
    }
    let outcome = if event == "oversize" {
        match data
            .get("utf8_len")
            .and_then(Value::as_u64)
            .filter(|size| *size > MAX_WIRE_BYTES as u64)
        {
            Some(size) => Ok(Err(format!(
                "Claude turn.complete wire exceeded the 4 MiB limit ({size} UTF-8 bytes); the answer was not truncated"
            ))),
            None => Err(anyhow::anyhow!(
                "invalid oversized Claude completion length"
            )),
        }
    } else if event == "turn.complete" {
        outcome(data, &pending.marker)
    } else {
        Err(anyhow::anyhow!("unsupported Claude module event"))
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return diagnostic(directory, event, &format!("{error:#}")),
    };
    drop(lock);
    let report = turn::Report::for_claim(&store, FirstPartyCli::Claude, Some(&token))
        .with_provenance(Some("claude_mod_turn_complete"));
    match outcome {
        Ok(answer) => report.complete(&answer, Some(conversation.into()), Some(pending.request_id)),
        Err(error) => report.fail(&error, Some(conversation.into()), Some(pending.request_id)),
    }
}

fn outcome(data: &Value, marker: &str) -> Result<Result<String, String>> {
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
            let answer = answer.strip_suffix(marker).unwrap_or(answer).trim_end();
            if answer.trim().is_empty() {
                Err("Claude ended the turn with no answer text".into())
            } else {
                Ok(answer.into())
            }
        }
        "aborted" => Err("Claude turn was interrupted".into()),
        "error" => Err(if answer.trim().is_empty() {
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
            Err(if details.is_empty() {
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
