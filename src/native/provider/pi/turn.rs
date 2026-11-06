//! Pi-owned pending turn state, framing and response correlation.
use super::hook::{pi_owned_string, pi_string, record_correlated_failure};
use super::usage::reported_usage;
use super::{input, startup_ready};
use crate::native::session::{Reader, Store, turn as session_turn};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const PENDING_TURN_FILE: &str = "pi-pending-turn.json";
pub(super) const CANCEL_ACCEPTED_FILE: &str = "pi-cancel-accepted.json";
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct PendingPiTurn {
    pub(super) schema: u32,
    pub(super) claim_token: String,
    pub(super) marker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) windows_input: Option<input::Fingerprint>,
}

impl PendingPiTurn {
    pub(super) fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-pi-turn:{claim_token} -->"),
            windows_input: None,
        })
    }
}

pub(super) fn validate_claim_token(claim_token: &str) -> Result<()> {
    if !session_turn::valid_turn_claim_token(claim_token) {
        bail!("invalid Pi turn claim token")
    }
    Ok(())
}

pub(super) fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingPiTurn> {
    install_pending_turn_bound(directory, claim_token, None)
}

pub(super) fn install_pending_turn_bound(
    directory: &Path,
    claim_token: &str,
    windows_input: Option<input::Fingerprint>,
) -> Result<PendingPiTurn> {
    let mut pending = PendingPiTurn::new(claim_token)?;
    pending.windows_input = windows_input;
    let store = Store::open_unchecked(directory);
    // Both installations (the launch plan while the session is Launching, and a
    // follow-up before delivery) require the current Claim of a session that has no
    // close tombstone and whose stored state is not ended; the rollback cancel below
    // checks the token only, because cleanup must run in any state.
    let result = (session_turn::ClaimCurrency {
        expected: Some(claim_token),
        state: session_turn::ClaimState::NotEnded,
        tombstone: session_turn::ClaimTombstone::Refused,
        expires: None,
    })
    .run(&store, || {
        store.private(CANCEL_ACCEPTED_FILE).remove()?;
        // Startup proves the installed extension's capability for the whole session.
        // Carry that evidence to a follow-up claim before delivery, including after a
        // rolled-back delivery; never upgrade a legacy extension's schema.
        startup_ready::carry_forward(&store, claim_token)?;
        store.private(PENDING_TURN_FILE).write_json(&pending)?;
        Ok(pending)
    })?;
    match result {
        Ok(pending) => Ok(pending),
        Err(session_turn::StaleClaim::Changed) => {
            bail!("Pi pending turn no longer holds the session claim")
        }
        Err(_) => bail!("Pi session ended; no pending turn installed"),
    }
}

pub(super) fn read_pending_turn(directory: &Path) -> Result<Option<PendingPiTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .timeline(4096)?
    else {
        return Ok(None);
    };
    let pending: PendingPiTurn =
        serde_json::from_slice(&text).context("failed to parse the pending Pi turn")?;
    let expected = PendingPiTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Pi turn correlation state")
    }
    if let Some(fingerprint) = &pending.windows_input {
        fingerprint.validate()?;
    }
    Ok(Some(pending))
}

pub(super) fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    if (session_turn::ClaimCurrency {
        expected: Some(claim_token),
        state: session_turn::ClaimState::Any,
        tombstone: session_turn::ClaimTombstone::Ignored,
        expires: None,
    })
    .check(&store)?
    .is_err()
    {
        return Ok(());
    }
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    store.private(CANCEL_ACCEPTED_FILE).remove()?;
    store.private(PENDING_TURN_FILE).remove()
}

pub(super) fn correlated_prompt(prompt: &str, pending: &PendingPiTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Pi turn protocol]\nCorrelation marker for the Agent Bridge extension:\n{}",
        pending.marker
    )
}

pub(super) fn terminal_correlated_prompt(
    prompt: &str,
    pending: &PendingPiTurn,
    windows: bool,
) -> Result<String> {
    if !windows {
        return Ok(correlated_prompt(prompt, pending));
    }
    let encoded = serde_json::to_string(prompt)?;
    Ok(format!(
        "[Agent Bridge Pi Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs, and complete it as one turn. Request JSON: {encoded} Correlation marker for the Agent Bridge extension: {}",
        pending.marker
    ))
}

pub(super) fn correlated_response<'a>(
    message: &'a str,
    pending: &PendingPiTurn,
) -> Result<&'a str> {
    let message = message.trim_end();
    let body = message
        .strip_suffix(&pending.marker)
        .unwrap_or(message)
        .trim_end();
    if body.is_empty() {
        bail!("Pi correlated response contained no assistant text")
    }
    Ok(body)
}

pub(super) fn handle_hook(directory: &Path, payload: &serde_json::Value) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pi_string(payload, "agent_bridge_claim_token") != Some(pending.claim_token.as_str()) {
        return Ok(());
    }
    if payload
        .get("agent_bridge_prompt_correlated")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Ok(());
    }
    let provider_session_id = pi_owned_string(payload, "session_id");
    let turn_id = pi_owned_string(payload, "turn_id");
    // Usage is information beside the outcome; a malformed usage never changes it.
    let usage = reported_usage(payload);
    if let Some(error) = pi_string(payload, "agent_bridge_error")
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        if pi_string(payload, "agent_bridge_cancel_claim_token")
            == Some(pending.claim_token.as_str())
        {
            // Report rechecks cancel.json under the lifecycle lock. Missing,
            // damaged or different-claim intent remains an ordinary failure.
            return session_turn::Report::for_claim(
                &Store::open_unchecked(directory),
                FirstPartyCli::Pi,
                Some(&pending.claim_token),
            )
            .with_usage(usage.usage, usage.usage_error)
            .cancelled(error, provider_session_id, turn_id)
            .context("failed to record the correlated Pi interruption");
        }
        return record_correlated_failure(
            directory,
            error,
            provider_session_id,
            turn_id,
            &pending,
            usage,
        );
    }
    let raw_message = pi_string(payload, "last_assistant_message")
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .context("Pi hook payload has no assistant result")?;
    let Ok(message) = correlated_response(raw_message, &pending) else {
        return Ok(());
    };
    session_turn::Report::for_claim(
        &Store::open_unchecked(directory),
        FirstPartyCli::Pi,
        Some(&pending.claim_token),
    )
    .with_usage(usage.usage, usage.usage_error)
    .complete(message, provider_session_id, turn_id)
    .context("failed to record the correlated Pi result")
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct HookFailureSignal {
    pub(super) error: String,
    pub(super) provider_session_id: Option<String>,
    pub(super) turn_id: Option<String>,
    #[serde(default)]
    pub(super) claim_token: Option<String>,
    /// What the extension's payload reported, kept when delivery failed; the same shape and
    /// validation as the hook payload (`usage` or a bounded `usage_error`). Both are kept raw
    /// so that a malformed field never rejects the signal before its claim is checked:
    /// [`reported_usage`] bounds them after the claim matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_error: Option<serde_json::Value>,
}

pub(super) fn decode_failure_signal(text: &str) -> Result<HookFailureSignal> {
    let signal: HookFailureSignal =
        serde_json::from_str(text).context("invalid Pi hook failure recovery signal")?;
    if signal.error.trim().is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    Ok(signal)
}
