use super::PendingPiTurn;
use super::usage::ReportedUsage;
use crate::native::session::{Store, turn};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result};
use std::path::Path;

pub(super) fn record_correlated_failure(
    directory: &Path,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    pending: &PendingPiTurn,
    usage: ReportedUsage,
) -> Result<()> {
    turn::Report::for_claim(
        &Store::open_unchecked(directory),
        FirstPartyCli::Pi,
        Some(&pending.claim_token),
    )
    .with_usage(usage.usage, usage.usage_error)
    .fail(error, provider_session_id, turn_id)
    .context("failed to record the correlated Pi failure")
}

pub(super) fn pi_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

pub(super) fn pi_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    pi_string(payload, key).map(str::to_owned)
}

#[cfg(test)]
mod tests;
