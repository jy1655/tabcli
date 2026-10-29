//! Doctor's read-only view of the completion module's own records. Each record is one
//! bounded read without the lifecycle lock, so the rows are not one snapshot. Nothing
//! here repairs, locks, starts a helper, reads provider policy or sends anything.
use super::{
    DIAGNOSTIC_FILE, Diagnostic, LaunchMode, MODE_FILE, Mode, READY_FILE, REVISION, Ready,
    valid_claude_conversation_id,
};
use crate::native::diagnostics::{
    Availability::{self, Unavailable, Unknown},
    Check,
};
use crate::native::session::Reader;
use agent_bridge::terminal_safe_text;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::Path;

// The records hold a few short fields; a diagnostic holds two 1024-character texts.
const RECORD_BYTES: u64 = 16 * 1024;
const TEXT_CHARS: usize = 1024;
const NAME_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    NotObserved,
    Observed,
    Unrecognized,
    Malformed,
    Unreadable,
}

struct Field {
    state: State,
    evidence: Value,
}

impl Field {
    fn new(state: State, file: &str, mut facts: Value) -> Self {
        facts["file"] = json!(file);
        facts["observation"] = json!(match state {
            State::NotObserved => "not_observed",
            State::Observed => "observed",
            State::Unrecognized => "unrecognized",
            State::Malformed => "malformed",
            State::Unreadable => "unreadable",
        });
        Self {
            state,
            evidence: facts,
        }
    }
}

// Record text is untrusted even though Bridge wrote it: echo only a sanitized excerpt.
fn excerpt(text: &str, limit: usize) -> (String, bool) {
    let safe = terminal_safe_text(text, false);
    let truncated = safe.chars().count() > limit;
    (safe.chars().take(limit).collect(), truncated)
}

fn sanitized(text: &str, limit: usize) -> String {
    excerpt(text, limit).0
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// Decodes exactly as the live reader does (the same struct straight from the bytes).
/// Only after that fails is the generic JSON inspected, to name an unknown schema,
/// revision or mode instead of calling it malformed.
fn read<T: DeserializeOwned>(
    reader: &Reader,
    file: &str,
    classify: impl FnOnce(&Value) -> Option<Value>,
) -> Result<Option<T>, Field> {
    let bytes = match reader.private(file).timeline(RECORD_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) => {
            return Err(Field::new(
                State::Unreadable,
                file,
                json!({"reason": sanitized(&format!("{error:#}"), TEXT_CHARS), "limit_bytes": RECORD_BYTES}),
            ));
        }
    };
    if let Ok(record) = serde_json::from_slice::<T>(&bytes) {
        return Ok(Some(record));
    }
    Err(match serde_json::from_slice::<Value>(&bytes) {
        Err(error) => Field::new(State::Malformed, file, json!({"fault": fault(&error)})),
        Ok(value) => match classify(&value) {
            Some(facts) => Field::new(State::Unrecognized, file, facts),
            None => Field::new(State::Malformed, file, json!({"fault": "schema_mismatch"})),
        },
    })
}

fn fault(error: &serde_json::Error) -> &'static str {
    use serde_json::error::Category;
    match error.classify() {
        Category::Syntax => "invalid_json",
        Category::Eof => "truncated_json",
        Category::Data => "schema_mismatch",
        Category::Io => "io",
    }
}

fn unknown_schema(value: &Value) -> Option<Value> {
    number(value, "schema")
        .filter(|schema| *schema != 1)
        .map(|schema| json!({"schema": schema, "expected_schema": 1}))
}

fn observe_mode(reader: &Reader) -> (Field, Option<Mode>) {
    // A schema-1 record whose mode is text that `Mode` does not name is unrecognized.
    let record = read::<LaunchMode>(reader, MODE_FILE, |value| {
        unknown_schema(value).or_else(|| {
            let mode = value.get("mode")?.as_str()?;
            (number(value, "schema") == Some(1)
                && serde_json::from_value::<Mode>(json!(mode)).is_err())
            .then(|| json!({"schema": 1, "mode": sanitized(mode, NAME_CHARS)}))
        })
    });
    match record {
        Ok(None) => (Field::new(State::NotObserved, MODE_FILE, json!({})), None),
        Err(field) => (field, None),
        Ok(Some(mode)) if mode.schema != 1 => (
            Field::new(
                State::Unrecognized,
                MODE_FILE,
                json!({"schema": mode.schema, "expected_schema": 1}),
            ),
            None,
        ),
        Ok(Some(LaunchMode {
            mode,
            reason,
            claude_version,
            ..
        })) => (
            Field::new(
                State::Observed,
                MODE_FILE,
                json!({
                    "schema": 1,
                    "mode": mode,
                    "reason": sanitized(&reason, TEXT_CHARS),
                    "claude_version_at_launch": claude_version.map(|version| sanitized(&version, TEXT_CHARS)),
                }),
            ),
            Some(mode),
        ),
    }
}

fn unknown_revision(schema: u64, revision: Option<u64>) -> Value {
    json!({"schema": schema, "revision": revision, "expected_schema": 1, "expected_revision": REVISION})
}

fn observe_readiness(reader: &Reader) -> Field {
    let record = read::<Ready>(reader, READY_FILE, |value| {
        match (number(value, "schema")?, number(value, "revision")) {
            (1, Some(revision)) if revision != u64::from(REVISION) => {
                Some(unknown_revision(1, Some(revision)))
            }
            (1, _) => None,
            (schema, revision) => Some(unknown_revision(schema, revision)),
        }
    });
    match record {
        Ok(None) => Field::new(State::NotObserved, READY_FILE, json!({})),
        Err(field) => field,
        Ok(Some(ready)) if ready.schema != 1 || ready.revision != REVISION => Field::new(
            State::Unrecognized,
            READY_FILE,
            unknown_revision(ready.schema.into(), Some(ready.revision.into())),
        ),
        Ok(Some(ready)) if !valid_claude_conversation_id(&ready.conversation) => Field::new(
            State::Malformed,
            READY_FILE,
            json!({"fault": "invalid_conversation"}),
        ),
        // The conversation of the latest recorded session.start; `/clear` can change the
        // live conversation without a new readiness record, so it names no current turn.
        Ok(Some(ready)) => Field::new(
            State::Observed,
            READY_FILE,
            json!({"schema": 1, "revision": REVISION, "session_start_conversation": ready.conversation}),
        ),
    }
}

fn observe_diagnostic(reader: &Reader) -> Field {
    match read::<Diagnostic>(reader, DIAGNOSTIC_FILE, unknown_schema) {
        Ok(None) => Field::new(State::NotObserved, DIAGNOSTIC_FILE, json!({})),
        Err(field) => field,
        Ok(Some(diagnostic)) if diagnostic.schema != 1 => Field::new(
            State::Unrecognized,
            DIAGNOSTIC_FILE,
            json!({"schema": diagnostic.schema, "expected_schema": 1}),
        ),
        Ok(Some(diagnostic)) => {
            let (event, event_cut) = excerpt(&diagnostic.event, TEXT_CHARS);
            let (reason, reason_cut) = excerpt(&diagnostic.reason, TEXT_CHARS);
            Field::new(
                State::Observed,
                DIAGNOSTIC_FILE,
                json!({"schema": 1, "event": event, "reason": reason, "truncated": event_cut || reason_cut}),
            )
        }
    }
}

const KEEP_RECORDS: &str = "Keep the records; doctor repairs, deletes and rewrites nothing. Inspect the session directory and the managed surface, and close the session if it cannot be used.";
const SAME_BINARY: &str = "Use the Bridge binary that launched this session; do not downgrade the binary or AGENT_BRIDGE_EXECUTABLE while native-mode Claude sessions remain open.";
const LEGACY_RECOVERY: &str = "Inspect the result and terminal, then close when appropriate. On macOS start a fresh ask with the needed context; on native Windows close and reopen to use native completion.";

type Verdict = (Availability, &'static str, &'static str, &'static str);

// Known rejected records can explain a refusal. Valid historical readiness does not
// establish live availability; delivery admission still reads its own records and gates.
fn verdict(mode: State, selected: Option<Mode>, readiness: State) -> Verdict {
    match (mode, selected, readiness) {
        (State::Unreadable, ..) => (
            Unknown,
            "claude_completion_mode_unreadable",
            "The completion-mode record could not be read within doctor's bounded reader, so the session's completion protocol is not known from this read.",
            KEEP_RECORDS,
        ),
        (State::Malformed, ..) => (
            Unavailable,
            "claude_completion_mode_malformed",
            "The completion-mode record does not decode as its schema; this binary refuses new requests for the session while it stays so.",
            KEEP_RECORDS,
        ),
        (State::Unrecognized, ..) => (
            Unavailable,
            "claude_completion_mode_unrecognized",
            "The completion-mode record names a schema or mode this binary does not know, for example one written by a newer binary; this binary refuses new requests for the session.",
            SAME_BINARY,
        ),
        (State::NotObserved, ..) => (
            Unavailable,
            "claude_completion_mode_absent",
            "No completion-mode record: Bridge treats the session as launched before native per-turn completion and never upgrades it from a version. A pending Stop completion stays readable; new tells are refused.",
            LEGACY_RECOVERY,
        ),
        (State::Observed, Some(Mode::LegacyStop), _) => (
            Unavailable,
            "claude_completion_legacy_stop",
            "The session records classic Stop completion. A pending Stop completion stays readable; new tells are refused.",
            LEGACY_RECOVERY,
        ),
        // Only native_v1 remains: an observed mode always carries its value.
        (_, _, State::Observed) => (
            Unknown,
            "claude_completion_readiness_recorded",
            "The session records native_v1 completion and the module's session.start readiness, the records delivery admission reads. They do not prove that the module is still loaded, that messaging works or that a request will complete; admission reads them again under the current claim.",
            "No action from these records. Use inspect for a retained unmarked candidate and the managed surface for the live state; doctor sends nothing.",
        ),
        (_, _, State::NotObserved) => (
            Unknown,
            "claude_completion_readiness_not_observed",
            "native_v1 completion was selected at launch from the recorded Claude version, but no session.start readiness is recorded. The module may still be loading, or hooks, plugin trust or provider policy may have kept it from running; the version and settings do not prove it ran. Delivery waits for readiness and refuses without it, sending nothing.",
            "Inspect module load, plugin trust and provider policy, such as disableAllHooks, in the managed surface; doctor reads and changes none of them.",
        ),
        (_, _, State::Unrecognized) => (
            Unavailable,
            "claude_completion_readiness_unrecognized",
            "The readiness record has a schema or revision this binary does not accept; delivery refuses it and sends nothing.",
            SAME_BINARY,
        ),
        (_, _, State::Malformed) => (
            Unavailable,
            "claude_completion_readiness_malformed",
            "The readiness record does not decode as its schema or names an invalid conversation; delivery refuses it and sends nothing.",
            KEEP_RECORDS,
        ),
        (_, _, State::Unreadable) => (
            Unknown,
            "claude_completion_readiness_unreadable",
            "The readiness record could not be read within doctor's bounded reader; readiness is not known from this read.",
            KEEP_RECORDS,
        ),
    }
}

fn completion(directory: Option<&Path>) -> Check {
    let Some(directory) = directory else {
        return Check::new(
            "claude_completion",
            Unknown,
            "session_required",
            "Completion mode and module readiness are records of one managed session; provider-only doctor reads none.",
            "Run doctor with a Claude session id to read its completion records.",
        )
        .evidence(json!({"source": "no_session_scope",
            "mode": {"observation": "not_read"}, "readiness": {"observation": "not_read"}}));
    };
    let reader = Reader::open_unchecked(directory);
    let (mode, selected) = observe_mode(&reader);
    let readiness = observe_readiness(&reader);
    let (availability, reason, detail, next_action) =
        verdict(mode.state, selected, readiness.state);
    Check::new(
        "claude_completion",
        availability,
        reason,
        detail,
        next_action,
    )
    .evidence(json!({
        "source": "session_records", "mode": mode.evidence, "readiness": readiness.evidence,
    }))
}

fn diagnostic(directory: Option<&Path>) -> Check {
    let Some(directory) = directory else {
        return Check::new(
            "claude_completion_diagnostic",
            Unknown,
            "session_required",
            "The completion module's latest rejection is a record of one managed session; provider-only doctor reads none.",
            "Run doctor with a Claude session id to read its completion records.",
        )
        .evidence(json!({"source": "no_session_scope", "latest_diagnostic": {"observation": "not_read"}}));
    };
    let field = observe_diagnostic(&Reader::open_unchecked(directory));
    let (reason, detail, next_action) = match field.state {
        State::Observed => (
            "claude_completion_diagnostic_recorded",
            "The latest signal the completion module rejected in this session, as sanitized and bounded untrusted text. The record carries no request id and no time: it cannot identify which request was rejected or show that the rejection is recent.",
            "Read it beside the managed surface and the exact request's result; never resend a request because of it.",
        ),
        State::NotObserved => (
            "claude_completion_diagnostic_not_recorded",
            "No completion-module rejection is recorded. Absence does not show that no signal was rejected, that the module ran, or that a request will complete.",
            "Inspect the exact request's result and the managed surface; doctor sends nothing.",
        ),
        State::Unrecognized => (
            "claude_completion_diagnostic_unrecognized",
            "The latest diagnostic record has a schema this binary does not know; its text is not shown.",
            SAME_BINARY,
        ),
        State::Malformed => (
            "claude_completion_diagnostic_malformed",
            "The latest diagnostic record does not decode as its schema; its text is not shown.",
            KEEP_RECORDS,
        ),
        State::Unreadable => (
            "claude_completion_diagnostic_unreadable",
            "The latest diagnostic record could not be read within doctor's bounded reader.",
            KEEP_RECORDS,
        ),
    };
    // A rejection is history without a request identity, so it never decides availability.
    Check::new(
        "claude_completion_diagnostic",
        Unknown,
        reason,
        detail,
        next_action,
    )
    .evidence(json!({"source": "session_records", "latest_diagnostic": field.evidence}))
}

pub(in crate::native::provider::claude) fn checks(directory: Option<&Path>) -> [Check; 2] {
    [completion(directory), diagnostic(directory)]
}

#[cfg(test)]
pub(super) fn record_limit() -> u64 {
    RECORD_BYTES
}
