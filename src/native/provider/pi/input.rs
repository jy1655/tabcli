use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Fingerprint {
    sha256: String,
    utf8_bytes: u64,
}

impl Fingerprint {
    #[cfg(any(windows, test))]
    pub(super) fn of(prompt: &str) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            sha256: format!("{:x}", Sha256::digest(prompt.as_bytes())),
            utf8_bytes: prompt.len() as u64,
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.utf8_bytes == 0
            || self.utf8_bytes > 9_007_199_254_740_991
        {
            bail!("invalid Pi Windows input fingerprint");
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
use super::{PendingPiTurn, TurnContext};
#[cfg(windows)]
use super::{Prepared, STARTUP_READY_FILE, StartupReady, turn};
#[cfg(any(windows, test))]
use crate::native::session::Reader;
#[cfg(windows)]
use crate::native::terminal;
#[cfg(any(windows, test))]
use anyhow::Context;
#[cfg(any(windows, test))]
use std::path::Path;
#[cfg(windows)]
use std::{
    thread,
    time::{Duration, Instant},
};

#[cfg(any(windows, test))]
pub(super) fn prepare_windows(turn: &TurnContext<'_>, initial: bool) -> Result<String> {
    let pending = if initial {
        super::read_pending_turn(turn.directory)?.context("Pi initial turn is missing")?
    } else {
        PendingPiTurn::new(turn.claim_token)?
    };
    if pending.claim_token != turn.claim_token {
        bail!("Pi input does not identify this turn claim");
    }
    let prompt = super::terminal_correlated_prompt(turn.prompt, &pending, true)?;
    super::install_pending_turn_bound(
        turn.directory,
        turn.claim_token,
        Some(Fingerprint::of(&prompt)),
    )?;
    Ok(prompt)
}

#[cfg(any(windows, test))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Accepted {
    schema: u32,
    signal: String,
    claim_token: String,
    prompt_sha256: String,
    prompt_utf8_bytes: u64,
    session_id: String,
}

#[cfg(any(windows, test))]
fn valid_session_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

#[cfg(any(windows, test))]
pub(super) fn read_accepted(
    directory: &Path,
    claim: &str,
    fingerprint: &Fingerprint,
) -> Result<bool> {
    super::validate_claim_token(claim)?;
    #[cfg(all(windows, test))]
    super::test_support::before_receipt_read();
    let reader = Reader::open_unchecked(directory);
    let Some(bytes) = reader
        .private(&format!("pi-input-accepted-{claim}.json"))
        .timeline(4096)?
    else {
        return Ok(false);
    };
    let receipt: Accepted = serde_json::from_slice(&bytes).context("invalid Pi input receipt")?;
    if receipt.schema != 1
        || receipt.signal != "agent_start"
        || receipt.claim_token != claim
        || receipt.prompt_sha256 != fingerprint.sha256
        || receipt.prompt_utf8_bytes != fingerprint.utf8_bytes
        || !valid_session_id(&receipt.session_id)
    {
        bail!("Pi input receipt conflicts with the submitted frame");
    }
    Ok(true)
}

#[cfg(windows)]
fn current(turn: &TurnContext<'_>) -> Result<()> {
    let reader = Reader::open_unchecked(turn.directory);
    if turn::current_claim_token(&reader)?.as_deref() != Some(turn.claim_token) {
        bail!("Pi turn claim retired or changed");
    }
    if reader.closed_if_present()?.is_some() || reader.status()?.state.ended() {
        bail!("Pi session ended");
    }
    Ok(())
}

#[cfg(windows)]
fn unconfirmed(reason: impl std::fmt::Display) -> terminal::TerminalSendResult {
    let detail: String = reason.to_string().chars().take(512).collect();
    Err(terminal::TerminalSendFailure::delivery_uncertain(
        anyhow::anyhow!(
            "Pi input submission is uncertain: {detail}. Inspect this request's result and the managed Pi editor before further input; do not resend. Close or diagnose the session if unresolved"
        ),
    ))
}

#[cfg(windows)]
pub(super) fn send_windows(
    turn: &TurnContext<'_>,
    prepared: &Prepared,
    path: &Path,
) -> terminal::TerminalSendResult {
    let fingerprint = Fingerprint::of(&prepared.prompt);
    let preflight = || -> Result<()> {
        super::validate_claim_token(turn.claim_token)?;
        let reader = Reader::open_unchecked(turn.directory);
        let capability = (|| -> Result<()> {
            let bytes = reader
                .private(STARTUP_READY_FILE)
                .timeline(4096)?
                .context("Pi startup receipt is missing")?;
            let ready: StartupReady = serde_json::from_slice(&bytes)?;
            if ready.schema != 2
                || ready.input_ack_schema != Some(1)
                || ready.claim_token != turn.claim_token
                || !valid_session_id(&ready.session_id)
            {
                bail!("Pi input acknowledgement capability is absent or unsupported");
            }
            Ok(())
        })();
        capability.context("this Pi session cannot acknowledge Windows input; start a new Bridge session; no console input was sent")?;
        current(turn)?;
        if Instant::now() >= turn.deadline {
            bail!("Pi input deadline expired before console input; nothing was sent");
        }
        Ok(())
    };
    preflight().map_err(terminal::TerminalSendFailure::not_sent)?;
    if let Err(failure) = super::send_console_file(turn.surface, path, turn.deadline)
        && !failure.delivery_may_have_occurred()
    {
        return Err(failure);
    }
    loop {
        if Instant::now() >= turn.deadline {
            return unconfirmed("no matching agent_start receipt before the request deadline");
        }
        match read_accepted(turn.directory, turn.claim_token, &fingerprint) {
            Ok(true) if Instant::now() < turn.deadline => return Ok(()),
            Ok(_) => {}
            Err(error) => return unconfirmed(error),
        }
        if let Err(error) = current(turn) {
            #[cfg(test)]
            super::test_support::after_retirement();
            return match read_accepted(turn.directory, turn.claim_token, &fingerprint) {
                Ok(true) if Instant::now() < turn.deadline => Ok(()),
                Ok(_) => unconfirmed(error),
                Err(error) => unconfirmed(error),
            };
        }
        thread::sleep(
            turn.deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    }
}
