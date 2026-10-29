//! Pi's startup-readiness record: the one place that reads it, decides whether it is
//! usable, answers the capability questions about it and carries it to a later Claim.
//!
//! The session-local extension writes the record at its `session_start` hook, naming the
//! Claim it saw and the schema of the turn capabilities it offers (schema 2 adds `cancel`;
//! `input_ack_schema` 1 adds Windows input acknowledgement). Bridge never infers any of
//! this from a Pi version. Every reader goes through [`read`], so one bound, one validity
//! floor and one session-id rule hold for the initial-paste gate, the Windows input
//! preflight, `cancel` support and the follow-up carry-forward. The receipt reader in
//! `input.rs` shares [`valid_session_id`] and nothing else.
use super::validate_claim_token;
use crate::native::session::{Reader, Store};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub(super) const STARTUP_READY_FILE: &str = "pi-startup-ready.json";

/// The record is small; a larger file is not one the extension wrote.
const READ_LIMIT: u64 = 4096;

/// The record as the extension writes it. Serde attributes are the wire contract with
/// `pi_extension.js`.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct StartupReady {
    pub(super) schema: u32,
    pub(super) claim_token: String,
    pub(super) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) input_ack_schema: Option<u32>,
}

/// What one bounded read of the record found.
#[derive(Debug)]
pub(super) enum StartupReadiness {
    /// No record: the extension has not reached `session_start`, or the session has none.
    Absent,
    /// A record Bridge cannot use: unreadable, over the bound, malformed JSON, or one that
    /// fails the validity floor (schema outside 1..=2, an invalid claim token, an invalid
    /// session id). Callers choose whether that is a refusal or merely "unsupported".
    Unreadable(anyhow::Error),
    /// A record that passed the floor. Ask it the questions below against the expected Claim.
    Present(StartupReady),
}

/// Pi's own session id as the extension reports it: non-blank, at most 256 bytes, without
/// control characters. The startup record and the Windows input receipt share this rule.
pub(super) fn valid_session_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

/// Reads the record within its bound and applies the validity floor once. This never
/// reads the Claim; callers supply the token they expect to the questions.
pub(super) fn read(reader: &Reader) -> StartupReadiness {
    let bytes = match reader.private(STARTUP_READY_FILE).timeline(READ_LIMIT) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return StartupReadiness::Absent,
        Err(error) => return StartupReadiness::Unreadable(error),
    };
    let ready: StartupReady = match serde_json::from_slice(&bytes) {
        Ok(ready) => ready,
        Err(error) => return StartupReadiness::Unreadable(error.into()),
    };
    match floor(&ready) {
        Ok(()) => StartupReadiness::Present(ready),
        Err(error) => StartupReadiness::Unreadable(error),
    }
}

fn floor(ready: &StartupReady) -> Result<()> {
    if !matches!(ready.schema, 1 | 2) {
        bail!("Pi startup receipt schema {} is unsupported", ready.schema);
    }
    validate_claim_token(&ready.claim_token)?;
    if !valid_session_id(&ready.session_id) {
        bail!("Pi startup receipt session id is invalid");
    }
    Ok(())
}

impl StartupReady {
    /// The extension reached `session_start` for `claim_token`: the initial paste may go.
    #[cfg(any(windows, test))]
    pub(super) fn resolves_startup_for(&self, claim_token: &str) -> bool {
        matches!(self.schema, 1 | 2) && self.claim_token == claim_token
    }

    /// The installed extension offers `cancel` for `claim_token`.
    pub(super) fn supports_cancel_for(&self, claim_token: &str) -> bool {
        self.schema == 2 && self.claim_token == claim_token
    }

    /// The installed extension acknowledges Windows input for `claim_token`.
    #[cfg(any(windows, test))]
    pub(super) fn acknowledges_windows_input_for(&self, claim_token: &str) -> bool {
        self.schema == 2 && self.input_ack_schema == Some(1) && self.claim_token == claim_token
    }
}

/// Carries the extension's startup evidence to `claim_token`, the Claim the caller holds
/// under the lifecycle lock. Schema 2 is rewritten with the new token and nothing else
/// changed; schema 1 stays as written (a legacy extension is never upgraded); an absent or
/// unreadable record is left alone.
pub(super) fn carry_forward(store: &Store, claim_token: &str) -> Result<()> {
    validate_claim_token(claim_token)?;
    let StartupReadiness::Present(mut ready) = read(store) else {
        return Ok(());
    };
    if ready.schema != 2 {
        return Ok(());
    }
    ready.claim_token = claim_token.to_owned();
    store.private(STARTUP_READY_FILE).write_json(&ready)
}

#[cfg(test)]
mod tests;
