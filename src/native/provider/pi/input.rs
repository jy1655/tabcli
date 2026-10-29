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
use super::PendingPiTurn;
use super::TurnContext;
#[cfg(any(windows, test))]
use super::startup_ready::valid_session_id;
#[cfg(windows)]
use super::startup_ready::{self, StartupReadiness};
#[cfg(windows)]
use super::{Prepared, turn};
#[cfg(windows)]
use crate::native::clock::{Clock, SystemClock};
#[cfg(any(windows, test))]
use crate::native::session::Reader;
use crate::native::terminal;
#[cfg(any(windows, test))]
use anyhow::Context;
use std::path::Path;
#[cfg(windows)]
use std::time::Duration;

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
pub(super) fn read_accepted(
    directory: &Path,
    claim: &str,
    fingerprint: &Fingerprint,
) -> Result<bool> {
    super::validate_claim_token(claim)?;
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
pub(super) fn current(turn: &TurnContext<'_>) -> Result<()> {
    let reader = Reader::open_unchecked(turn.directory);
    match (turn::ClaimCurrency {
        expected: Some(turn.claim_token),
        state: turn::ClaimState::NotEnded,
        tombstone: turn::ClaimTombstone::Refused,
        expires: None,
    })
    .check(&reader)?
    {
        Ok(()) => {}
        Err(turn::StaleClaim::Changed) => bail!("Pi turn claim retired or changed"),
        Err(_) => bail!("Pi session ended"),
    }
    Ok(())
}

// Pi's input operation owns the ordering of observations. Its runtime supplies only
// effects, so tests can move time or retire a Claim during an actual receipt read
// without changing the protocol or installing thread-local test checkpoints.
pub(super) trait Runtime {
    #[cfg(windows)]
    fn clock(&mut self) -> &mut dyn Clock;
    fn send(&mut self, turn: &TurnContext<'_>, path: &Path) -> terminal::TerminalSendResult;
    #[cfg(windows)]
    fn accepted(&mut self, turn: &TurnContext<'_>, fingerprint: &Fingerprint) -> Result<bool>;
    #[cfg(windows)]
    fn current(&mut self, turn: &TurnContext<'_>) -> Result<()>;
}

#[cfg_attr(not(windows), derive(Default))]
pub(super) struct Console {
    #[cfg(windows)]
    clock: SystemClock,
}

#[cfg(windows)]
impl Default for Console {
    fn default() -> Self {
        Self { clock: SystemClock }
    }
}

impl Runtime for Console {
    #[cfg(windows)]
    fn clock(&mut self) -> &mut dyn Clock {
        &mut self.clock
    }
    fn send(&mut self, turn: &TurnContext<'_>, path: &Path) -> terminal::TerminalSendResult {
        terminal::send_file(turn.surface, path, turn.deadline)
    }
    #[cfg(windows)]
    fn accepted(&mut self, turn: &TurnContext<'_>, fingerprint: &Fingerprint) -> Result<bool> {
        read_accepted(turn.directory, turn.claim_token, fingerprint)
    }
    #[cfg(windows)]
    fn current(&mut self, turn: &TurnContext<'_>) -> Result<()> {
        current(turn)
    }
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
    runtime: &mut impl Runtime,
) -> terminal::TerminalSendResult {
    let fingerprint = Fingerprint::of(&prepared.prompt);
    let mut preflight = || -> Result<()> {
        super::validate_claim_token(turn.claim_token)?;
        let reader = Reader::open_unchecked(turn.directory);
        let capability = (|| -> Result<()> {
            let ready = match startup_ready::read(&reader) {
                StartupReadiness::Absent => bail!("Pi startup receipt is missing"),
                StartupReadiness::Unreadable(error) => return Err(error),
                StartupReadiness::Present(ready) => ready,
            };
            if !ready.acknowledges_windows_input_for(turn.claim_token) {
                bail!("Pi input acknowledgement capability is absent or unsupported");
            }
            Ok(())
        })();
        capability.context("this Pi session cannot acknowledge Windows input; start a new Bridge session; no console input was sent")?;
        runtime.current(turn)?;
        if runtime.clock().now() >= turn.deadline {
            bail!("Pi input deadline expired before console input; nothing was sent");
        }
        Ok(())
    };
    preflight().map_err(terminal::TerminalSendFailure::not_sent)?;
    if let Err(failure) = runtime.send(turn, path)
        && !failure.delivery_may_have_occurred()
    {
        return Err(failure);
    }
    loop {
        if runtime.clock().now() >= turn.deadline {
            return unconfirmed("no matching agent_start receipt before the request deadline");
        }
        match runtime.accepted(turn, &fingerprint) {
            Ok(true) if runtime.clock().now() < turn.deadline => return Ok(()),
            Ok(_) => {}
            Err(error) => return unconfirmed(error),
        }
        if let Err(error) = runtime.current(turn) {
            return match runtime.accepted(turn, &fingerprint) {
                Ok(true) if runtime.clock().now() < turn.deadline => Ok(()),
                Ok(_) => unconfirmed(error),
                Err(error) => unconfirmed(error),
            };
        }
        let remaining = turn
            .deadline
            .saturating_duration_since(runtime.clock().now());
        runtime
            .clock()
            .sleep(remaining.min(Duration::from_millis(50)));
    }
}
