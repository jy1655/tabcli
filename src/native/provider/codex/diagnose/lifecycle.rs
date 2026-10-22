//! Historical session evidence only; never effective live configuration.
use super::super::{codex_version, hooks, lifecycle};
use crate::native::diagnostics::{Availability, Check};
use crate::native::query::Selector;
use crate::native::query::request::RequestState;
use crate::native::session::schema::SessionEvent;
use crate::native::session::{Reader, requests, turn};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::Path;

fn absent() -> Value {
    json!({"observation":"not_observed"})
}
fn unreadable(error: impl std::fmt::Display) -> Value {
    json!({"observation":"unavailable","reason":error.to_string()})
}
fn observed(value: impl serde::Serialize) -> Value {
    json!({"observation":"observed","record":value})
}

pub(super) fn check(directory: Option<&Path>) -> Check {
    let mut evidence = json!({"source":"session_records", "claim_token":null,
        "launch_mode":absent(),"input_hook_observed":absent(),
        "stop_candidate_observed":absent(),"observer":absent(),"terminal_outcome":absent()});
    if let Some(directory) = directory {
        match hooks::read_launch_mode(directory) {
            Ok(Some(mode)) => {
                let differs = mode
                    .codex_version
                    .as_deref()
                    .and_then(|v| codex_version(v).ok())
                    .is_some_and(|v| v.to_string() != mode.vector_codex_version);
                evidence["launch_mode"] = observed(mode);
                if differs {
                    evidence["launch_mode"]["warning"] = json!(
                        "recorded Codex version differs from hash vector version; injection is unchanged"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => evidence["launch_mode"] = unreadable(error),
        }
        if let Err(error) = claim_evidence(directory, &mut evidence) {
            evidence["claim_selection"] = unreadable(error);
        }
    }
    Check::new("codex_lifecycle", Availability::Unknown,
        if directory.is_some() { "codex_lifecycle_session_observations" } else { "session_required" },
        "Recorded observations of this session only: injection is not hook execution; a Stop candidate and observer state are not a committed terminal outcome. No field asserts effective live configuration.",
        "Inspect missing or suspended observations and the managed surface. Use --probe separately for current hook trust; never resend uncertain input.")
        .evidence(evidence)
}

fn claim_evidence(directory: &Path, evidence: &mut Value) -> Result<()> {
    let reader = Reader::open_unchecked(directory);
    let index = requests::list(&reader)?;
    if index.unreadable != 0 {
        bail!("request index has unreadable receipts");
    }
    let last = index
        .receipts
        .iter()
        .max_by_key(|r| (r.created_unix_ms, &r.request_id));
    let current = turn::current_claim_token(&reader)?;
    let token = current
        .as_deref()
        .or_else(|| last.map(|r| r.claim_token.as_str()));
    if let Some(token) = token {
        if !turn::valid_turn_claim_token(token) {
            bail!("invalid claim token");
        }
        evidence["claim_token"] = json!(token);
        evidence["input_hook_observed"] = match lifecycle::read_binding(directory, token) {
            Ok(Some(binding)) => observed(binding),
            Ok(None) => absent(),
            Err(error) => unreadable(error),
        };
        evidence["stop_candidate_observed"] = sidecar(&reader, "codex-candidate-", token);
        evidence["observer"] = sidecar(&reader, "codex-observer-", token);
    }
    // Never substitute an older result or the observer's claim of settlement.
    if let Some(receipt) = last {
        evidence["terminal_outcome"] = match terminal(&reader, receipt) {
            Ok(value) => value,
            Err(error) => unreadable(error),
        };
    }
    Ok(())
}

fn sidecar(reader: &Reader, prefix: &str, token: &str) -> Value {
    let result = (|| -> Result<Option<Value>> {
        let value = reader
            .private(&format!("{prefix}{token}.json"))
            .optional_json::<Value>()?;
        if let Some(value) = &value {
            if value["schema"] != 1
                || value["claim_token"] != token
                || !value["observed_unix_ms"].is_u64()
            {
                bail!("invalid claim-addressed observation");
            }
            if prefix == "codex-observer-"
                && (!matches!(
                    value["state"].as_str(),
                    Some("armed" | "suspended" | "settled")
                ) || !(value["reason"].is_null() || value["reason"].is_string()))
            {
                bail!("invalid observer state");
            }
        }
        Ok(value)
    })();
    match result {
        Ok(Some(mut value)) => {
            value
                .as_object_mut()
                .unwrap()
                .remove("last_assistant_message");
            observed(value)
        }
        Ok(None) => absent(),
        Err(error) => unreadable(error),
    }
}

fn terminal(reader: &Reader, receipt: &requests::Receipt) -> Result<Value> {
    terminal_with(reader, receipt, |name| {
        reader.event_strict(name).map_err(anyhow::Error::msg)
    })
}

fn terminal_with(
    reader: &Reader,
    receipt: &requests::Receipt,
    read_event: impl FnOnce(&str) -> Result<SessionEvent>,
) -> Result<Value> {
    let state = {
        let snapshot = crate::native::query::observe_snapshot(reader)?;
        let selector = Selector::Request(receipt.request_id.clone());
        let result = snapshot.observe_result(reader, &selector)?;
        if !matches!(
            result.state(),
            RequestState::Completed | RequestState::Failed | RequestState::Cancelled
        ) || result.event_id() != Some(receipt.event_file.as_str())
        {
            return Ok(absent());
        }
        result.state()
    };
    let event = read_event(&receipt.event_file)?;
    if event.provenance.is_none() {
        return Ok(absent());
    }
    let kind = match event.provenance.as_deref() {
        Some("codex_transcript_task_complete") => "task_complete",
        Some("codex_transcript_turn_aborted") => "turn_aborted",
        Some("codex_notify") => "agent-turn-complete",
        _ => "not_observed",
    };
    Ok(observed(
        json!({"request_id":receipt.request_id,"event_id":receipt.event_file,
        "provenance":event.provenance,"kind":kind,"request_state":state,
        "created_unix_ms":event.created_unix_ms}),
    ))
}

#[cfg(test)]
mod tests;
