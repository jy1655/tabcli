use super::turn::{self as claude_turn, ClaudeTurn, Installed};
use super::valid_claude_conversation_id;
use crate::native::clock::Clock;
use crate::native::session::{Reader, Store, turn};
use crate::native::settings::{self, ClaudeQuestionDeflection};
use agent_bridge::{FirstPartyCli, terminal_safe_text};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

/// The question plugin's manifest that `prepare` writes for a native-mode launch.
const QUESTION_PLUGIN_MANIFEST: &str = "claude-question-mod/.claude-plugin/plugin.json";
/// The plugin name that manifest carries.
const QUESTION_PLUGIN_NAME: &str = "agent-bridge-questions";
const MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FROM_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 256;
const MAX_MODE_BYTES: usize = 32;
const MAX_TURN_BYTES: usize = 128;
const MODULE: &str = include_str!("completion.js");

mod diagnose;
mod questions;
mod records;
#[cfg(test)]
mod test_support;
pub(super) use diagnose::checks as doctor_checks;
pub(in crate::native::provider) use questions::read as question_emissions;
pub(super) use records::native_mode;
use records::{
    Binding, Candidate, Continuation, Deflection, Diagnostic, LaunchMode, QuestionMode, Ready,
};
#[cfg(test)]
pub(in crate::native) use test_support::CompletionRecordFixture;

enum Outcome {
    Answer { text: String, marked: bool },
    Failed(String),
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
    let mode = LaunchMode::native(windows, version);
    let store = Store::open_unchecked(directory);
    if let Some(existing) = records::read_live::<LaunchMode>(&store)? {
        if existing != mode {
            bail!("a different Claude completion mode was already recorded for this launch");
        }
    } else {
        records::write(&store, &mode)?;
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
    store
        .private("claude-mod/hooks/report.js")
        .write_private(include_bytes!("report.js"))?;
    // Claude 2.1.309 permits only one module per plugin. Keep optional question
    // observation in its own plugin so a module policy refusal preserves completion.
    let questions = directory.join("claude-question-mod");
    std::fs::create_dir_all(questions.join(".claude-plugin"))?;
    std::fs::create_dir_all(questions.join("hooks"))?;
    store.private(QUESTION_PLUGIN_MANIFEST).write_json(&json!({
        "name":QUESTION_PLUGIN_NAME, "version":"1.0.0", "description":"Observe official Claude question emissions for Agent Bridge."
    }))?;
    store
        .private("claude-question-mod/hooks/hooks.json")
        .write_json(&json!({"modules":["./register.js"]}))?;
    store
        .private("claude-question-mod/hooks/register.js")
        .write_private(include_bytes!("questions.js"))?;
    store
        .private("claude-question-mod/hooks/report.js")
        .write_private(include_bytes!("report.js"))?;
    // Decided once per launch from the user's setting. An unreadable settings record keeps
    // the default: no person is at a managed session's dialog by construction.
    let setting = directory
        .parent()
        .context("Claude session has no state root")
        .and_then(settings::claude_question_deflection);
    let deflection = match setting {
        Ok(ClaudeQuestionDeflection::On) => Deflection::On,
        Ok(ClaudeQuestionDeflection::Off) => Deflection::Off,
        Err(error) => {
            crate::native::session::launch::log(
                &store,
                &format!("settings could not be read, using question deflection on: {error:#}"),
            );
            Deflection::On
        }
    };
    store
        .private("claude-question-mod/hooks/config.js")
        .write_private(
            format!("export const deflect = {};\n", deflection == Deflection::On).as_bytes(),
        )?;
    // Written after the module switch, so the record never names a switch that was not written.
    records::write(&store, &QuestionMode::new(deflection))?;
    Ok(vec![
        "--plugin-dir".into(),
        plugin.into_os_string(),
        "--plugin-dir".into(),
        questions.into_os_string(),
    ])
}

pub(super) fn settings(_executable: &Path) -> Value {
    json!({"crossSessionInbound":"accept"})
}

/// The completion-mode gate a request passes before the readiness wait: None for native
/// completion, the refusal text for a session launched before it. `admit` and the inspect
/// observation (`follow_up_static`) both decide from this one function; an unreadable or
/// unsupported mode record is an error for both.
fn legacy_refusal(directory: &Path, windows: bool) -> Result<Option<&'static str>> {
    if native_mode(directory)? {
        return Ok(None);
    }
    Ok(Some(if windows {
        "Claude session was launched before native per-turn completion; close and reopen it before sending a new request"
    } else {
        "Claude session was launched before native per-turn completion; inspect its result and terminal, close it when appropriate, then start a fresh ask with the needed context; no message was sent"
    }))
}

/// The follow-up gate decided from the completion-mode record alone. Readiness is the
/// later wait in `admit`, which this observation names instead of evaluating.
pub(super) fn follow_up_static(
    directory: &Path,
    windows: bool,
) -> crate::native::provider::FollowUpStatic {
    use crate::native::provider::FollowUpStatic;
    match legacy_refusal(directory, windows) {
        Ok(None) => FollowUpStatic::Ok {
            unevaluated: &["claude_native_readiness"],
        },
        Ok(Some(refusal)) => FollowUpStatic::Refused {
            reason: "claude_legacy_completion",
            detail: refusal.to_owned(),
        },
        Err(error) => FollowUpStatic::Unknown(format!("{error:#}")),
    }
}

/// Question emissions are observed by the separate plugin that a native-mode launch
/// prepares. Native mode alone does not prove it: v0.4.25 recorded native mode before the
/// question plugin existed (v0.4.26). The prepared manifest is read, never written; its
/// presence does not prove that Claude loaded the plugin.
pub(super) fn emission_observation(
    directory: &Path,
) -> crate::native::provider::EmissionObservation {
    use crate::native::provider::EmissionObservation;
    let observe = || -> Result<EmissionObservation> {
        if !native_mode(directory)? {
            return Ok(EmissionObservation::Unavailable("legacy_completion_mode"));
        }
        let Some(bytes) = Reader::open_unchecked(directory)
            .private(QUESTION_PLUGIN_MANIFEST)
            .bytes()?
        else {
            return Ok(EmissionObservation::Unavailable(
                "question_plugin_not_prepared",
            ));
        };
        // Only the manifest `prepare` writes identifies the prepared observer; an empty,
        // truncated or foreign file is evidence that cannot be read as it.
        let manifest: Value = serde_json::from_slice(&bytes)
            .context("Claude question plugin manifest is not valid JSON")?;
        if manifest.get("name").and_then(Value::as_str) != Some(QUESTION_PLUGIN_NAME) {
            bail!("Claude question plugin manifest does not name {QUESTION_PLUGIN_NAME}");
        }
        Ok(EmissionObservation::Configured(
            "plugin_configured_unverified",
        ))
    };
    observe().unwrap_or_else(|error| EmissionObservation::Unknown(format!("{error:#}")))
}

/// The answer path this launch recorded. Deflection returns a refused question in the Result,
/// answered by an ordinary `tell`; the record proves neither that Claude loaded the plugin nor
/// that any call was refused. A session launched before the record existed, with deflection
/// off, or in legacy completion mode has no answer path.
pub(super) fn question_response(directory: &Path) -> crate::native::provider::QuestionResponse {
    use crate::native::provider::{NO_ADDRESSED_ANSWER_PATH, QuestionResponse};
    let observe = || -> Result<QuestionResponse> {
        if native_mode(directory)?
            && records::question_deflection(directory)? == Some(Deflection::On)
        {
            return Ok(QuestionResponse::DeflectedToResult(
                "plugin_configured_unverified",
            ));
        }
        Ok(QuestionResponse::Unsupported(NO_ADDRESSED_ANSWER_PATH))
    };
    observe().unwrap_or_else(|error| QuestionResponse::Unknown(format!("{error:#}")))
}

/// Whether Claude started past its launch dialogs, for a launch under bypass: the module's
/// `session.start` report is written only by a running session. A launch without native
/// completion has no such report.
pub(super) fn started_under_bypass(directory: &Path) -> Result<Option<bool>> {
    if !native_mode(directory)? {
        return Ok(None);
    }
    Ok(Some(
        records::read_live::<Ready>(&Reader::open_unchecked(directory))?.is_some(),
    ))
}

// What Claude's own store says when readiness never came. The store is evidence about the
// workspace, not about the screen: the sentence names the dialog as a possibility only.
pub(super) fn readiness_timeout(directory: &Path) -> String {
    readiness_timeout_with(directory, &crate::native::consent::Environment::current())
}

fn readiness_timeout_with(directory: &Path, env: &crate::native::consent::Environment) -> String {
    let mut message = String::from(
        "Claude completion module readiness was not observed before the delivery deadline; inspect module load, plugin trust and provider policy; no message was sent",
    );
    let Ok(manifest) = Reader::open_unchecked(directory).manifest() else {
        return message;
    };
    if matches!(
        super::trust::workspace_trust(&manifest.workspace, env, None),
        Ok(crate::native::consent::Trust::Absent | crate::native::consent::Trust::Declined)
    ) {
        message.push_str(&format!(
            "; Claude's own store records no trust for workspace {}, so the session may be waiting at Claude's workspace trust dialog: read it with `{} screen {}` and answer it in the managed terminal",
            terminal_safe_text(&manifest.workspace.to_string_lossy(), false),
            agent_bridge::PUBLIC_COMMAND,
            manifest.id
        ));
    }
    message
}

pub(super) fn admit(
    directory: &Path,
    token: &str,
    deadline: Instant,
    windows: bool,
    clock: &mut impl Clock,
) -> Result<()> {
    if let Some(refusal) = legacy_refusal(directory, windows)? {
        return Err(anyhow::Error::msg(refusal));
    }
    loop {
        claude_turn::require_current(&Reader::open_unchecked(directory), token)?;
        let remaining = deadline
            .checked_duration_since(clock.now())
            .filter(|remaining| !remaining.is_zero())
            .with_context(|| readiness_timeout(directory))?;
        if let Some(ready) = records::read_live::<Ready>(&Reader::open_unchecked(directory))? {
            if !ready.valid() {
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
    records::write(
        &Store::open_unchecked(directory),
        &Diagnostic {
            schema: 1,
            event: bounded(event),
            reason: bounded(reason),
        },
    )
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

fn remember_candidate(store: &Store, binding: &Binding, answer: &str) -> Result<()> {
    match Candidate::read(store, &binding.claim)? {
        Some(candidate) if candidate.matches(binding) => Ok(()),
        Some(_) => bail!("Claude completion candidate does not match the immutable binding"),
        None => Candidate::new(binding, bounded(answer)).write(store),
    }
}

// Called under the lifecycle lock, after the live claim, pending request, original
// binding, conversation and fully decoded marked answer have all been verified.
fn select_continuation(
    store: &Store,
    binding: &Binding,
    turn_id: &str,
) -> Result<Option<Continuation>> {
    let candidate = Candidate::read(store, &binding.claim)?
        .context("Claude continuation has no unmarked bound-turn candidate")?;
    if !candidate.matches(binding) {
        bail!("Claude continuation candidate does not match the immutable binding");
    }
    if let Some(selected) = Continuation::read(store, &binding.claim)? {
        if !selected.matches(binding) || selected.turn_id != turn_id {
            bail!("Claude continuation conflicts with the already selected completion identity");
        }
        return Ok(None);
    }
    // Preserve both official identities before publication. A helper retry can repeat
    // this selection, but another turn cannot replace it if publication was interrupted.
    Ok(Some(Continuation::new(binding, turn_id)))
}

// Completion's binding: the floor for this Claim, and the pending request's correlation id.
fn binding_matches_request(binding: &Binding, pending: &ClaudeTurn, token: &str) -> bool {
    binding.valid_for(token) && binding.correlation_id == pending.id()
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
        let Some(candidate) = Candidate::read(reader, token)? else {
            return Ok(None);
        };
        let pending = claude_turn::read_native(reader, token)?;
        let binding = Binding::require(reader, token)?;
        if !binding_matches_request(&binding, &pending, token) || !candidate.matches(&binding) {
            bail!("Claude candidate does not match the current request and original binding");
        }
        let selected = Continuation::read(reader, token)?;
        // Inspection's condition: the selection names a turn other than the bound one.
        if let Some(selected) = &selected
            && (!selected.matches(&binding)
                || !valid_identity(&selected.turn_id)
                || selected.turn_id == binding.turn_id)
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
        return records::write(&store, &Ready::new(conversation));
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
    let proposed = Binding::new(&token, pending.id(), conversation, turn_id);
    let binding = match Binding::read(&store, &token) {
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
            None => proposed.write(&store),
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
        if event == "question.emissions" || event == "question.deflected" {
            return Ok(());
        }
        return diagnostic(
            directory,
            event,
            "Claude module event has no matching turn.start binding for the pending request; unrelated turns and input without its own exact start cannot settle it",
        );
    };
    let bound = binding == proposed;
    if event == "question.emissions" || event == "question.deflected" {
        return if !bound {
            Ok(())
        } else if event == "question.emissions" {
            questions::publish(&store, &binding, data)
        } else {
            questions::deflect(&store, &binding, data)
        };
    }
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
            selection.write(&store)?;
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
