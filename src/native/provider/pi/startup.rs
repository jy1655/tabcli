#[cfg(any(windows, test))]
use super::read_pending_turn;
#[cfg(any(windows, test))]
use super::startup_ready::{self, StartupReadiness};
#[cfg(any(windows, test))]
use crate::native::session::{Reader, turn};
use crate::native::terminal;
#[cfg(any(windows, test))]
use anyhow::{Context, Result, bail};
use std::path::Path;
#[cfg(any(windows, test))]
use std::time::Instant;
#[cfg(any(windows, test))]
use std::{thread, time::Duration};

pub(super) fn send_initial_prompt(
    turn: &super::TurnContext<'_>,
    prepared: &super::Prepared,
    prompt_path: &Path,
    runtime: &mut impl super::input::Runtime,
) -> terminal::TerminalSendResult {
    #[cfg(windows)]
    {
        send_initial_prompt_after_startup(turn.directory, turn.deadline, || {
            super::input::send_windows(turn, prepared, prompt_path, runtime)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = prepared;
        runtime.send(turn, prompt_path)
    }
}

#[cfg(any(windows, test))]
fn send_initial_prompt_after_startup(
    directory: &Path,
    deadline: Instant,
    send: impl FnOnce() -> terminal::TerminalSendResult,
) -> terminal::TerminalSendResult {
    // Pi's session_start(startup) follows resolution of project_trust. A fixed delay
    // cannot prove that its trust dialog has gone away: Enter would choose a button.
    // This is readiness, not consent; Pi can reach session_start after declining trust.
    let wait = || -> Result<()> {
        let pending = read_pending_turn(directory)?.context("Pi initial turn is missing")?;
        loop {
            if Instant::now() >= deadline {
                bail!(
                    "Pi startup has not confirmed that project trust was resolved; no initial console input was sent. Resolve the prompt in the managed terminal, then close and start a new Bridge session if this request timed out"
                );
            }
            match (turn::ClaimCurrency {
                expected: Some(&pending.claim_token),
                state: turn::ClaimState::NotEnded,
                tombstone: turn::ClaimTombstone::Refused,
                expires: None,
            })
            .check(&Reader::open_unchecked(directory))?
            {
                Ok(()) => {}
                Err(turn::StaleClaim::Changed) => {
                    bail!("Pi initial turn is no longer claimed; no initial console input was sent")
                }
                Err(_) => bail!("Pi session ended; no initial console input was sent"),
            }
            match startup_ready::read(&Reader::open_unchecked(directory)) {
                StartupReadiness::Absent => {}
                StartupReadiness::Unreadable(error) => {
                    return Err(error.context(
                        "invalid Pi startup receipt; start a new Bridge session; no initial console input was sent",
                    ));
                }
                StartupReadiness::Present(ready) => {
                    if !ready.resolves_startup_for(&pending.claim_token) {
                        bail!("Pi startup receipt does not identify this initial turn");
                    }
                    return Ok(());
                }
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }
    };
    wait().map_err(terminal::TerminalSendFailure::not_sent)?;
    send()
}

#[cfg(test)]
mod tests;
