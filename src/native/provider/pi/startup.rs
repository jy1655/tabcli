#[cfg(any(windows, test))]
use super::{STARTUP_READY_FILE, StartupReady, read_pending_turn};
#[cfg(any(windows, test))]
use crate::native::session::{Reader, turn};
use crate::native::terminal;
#[cfg(any(windows, test))]
use anyhow::{Context, Result, bail};
use std::{path::Path, time::Instant};
#[cfg(any(windows, test))]
use std::{thread, time::Duration};

pub(super) fn send_initial_prompt(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> terminal::TerminalSendResult {
    #[cfg(windows)]
    {
        let directory = session
            .managed_session_id
            .as_deref()
            .context("Pi initial input has no managed session identity")
            .and_then(Reader::session_directory)
            .map_err(terminal::TerminalSendFailure::not_sent)?;
        send_initial_prompt_after_startup(&directory, deadline, || {
            terminal::send_file(session, prompt_path, deadline)
        })
    }
    #[cfg(not(windows))]
    terminal::send_file(session, prompt_path, deadline)
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
            if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
                != Some(pending.claim_token.as_str())
            {
                bail!("Pi initial turn is no longer claimed; no initial console input was sent");
            }
            if let Some(text) = Reader::open_unchecked(directory)
                .private(STARTUP_READY_FILE)
                .text()?
            {
                let ready: StartupReady =
                    serde_json::from_str(&text).context("invalid Pi startup receipt")?;
                if !matches!(ready.schema, 1 | 2)
                    || ready.claim_token != pending.claim_token
                    || ready.session_id.trim().is_empty()
                {
                    bail!("Pi startup receipt does not identify this initial turn");
                }
                return Ok(());
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
