use super::PendingCodexTurn;
use super::correlation::correlated_prompt;
use crate::native::session::Reader;
use crate::native::terminal;
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub(super) fn send_initial_prompt(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> terminal::TerminalSendResult {
    if cfg!(windows) {
        let directory = session
            .managed_session_id
            .as_deref()
            .context("Codex terminal has no managed session binding")
            .and_then(Reader::session_directory)
            .map_err(terminal::TerminalSendFailure::not_sent)?;
        // A screen read costs a helper process and the wait polls ten times a second,
        // so the screen is read once per `COMPOSER_POLL`.
        let mut last_read: Option<Instant> = None;
        // The caller's environment is read once per paste gate.
        let env = crate::native::consent::Environment::current();
        initial_readiness_with(
            &directory,
            deadline,
            &mut crate::native::clock::SystemClock,
            &mut || terminal::read_screen(session, deadline),
            || {
                crate::native::consent::wait_for_native_trust(
                    &directory,
                    FirstPartyCli::Codex,
                    deadline,
                    &env,
                    &mut || {
                        if last_read.is_some_and(|read| read.elapsed() < COMPOSER_POLL) {
                            return false;
                        }
                        last_read = Some(Instant::now());
                        terminal::read_screen(session, deadline)
                            .is_ok_and(|screen| legacy_composer_is_ready(&screen))
                    },
                )
            },
        )
        .map_err(terminal::TerminalSendFailure::not_sent)?;
    }
    terminal::send_file(session, prompt_path, deadline)?;
    if cfg!(windows) {
        // Temporary measure for issue #124
        let submission_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
        wait_for_submission_with(
            || terminal::read_screen(session, submission_deadline),
            submission_deadline,
            COMPOSER_POLL,
            Instant::now,
        )?;
    }
    Ok(())
}

// Empty chat composer: prompt glyph plus Codex's placeholder; see docs/verification/2026-10-02-windows.md.
const COMPOSER_ROW: &str = "› Ask Codex to do anything";
const COMPOSER_POLL: Duration = Duration::from_secs(1);

// The launch record, never mutable settings or a current probe, selects this gate.
// Positive composer evidence already supplies the legacy OR's readiness branch;
// requiring exact trust as well would break repository-root trust.
fn initial_readiness_with(
    directory: &Path,
    deadline: Instant,
    clock: &mut impl crate::native::clock::Clock,
    read_screen: &mut impl FnMut() -> Result<String>,
    legacy: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if !super::hooks::read_launch_mode(directory)?
        .is_some_and(|mode| mode.lifecycle_hooks == super::hooks::LifecycleHooks::Injected)
    {
        return legacy();
    }
    let mut last_screen = String::new();
    loop {
        if let Ok(status) = Reader::open_unchecked(directory).status()
            && status.state.ended()
        {
            anyhow::bail!(
                "the session is {} and no longer waits for the Codex composer; no initial console input was sent",
                status.state
            );
        }
        if clock.now() >= deadline {
            let picker = if hook_picker_present(&last_screen) {
                " The last screen contained Review hooks or Trust all and continue (hook-review picker); Bridge never answers this picker."
            } else {
                ""
            };
            anyhow::bail!(
                "Codex empty composer readiness was not observed; no initial console input was sent.{picker}"
            );
        }
        if let Ok(screen) = read_screen() {
            last_screen = screen;
            // Reading the surface can take time: recheck the session and deadline
            // before accepting its evidence, not only before starting the read.
            if let Ok(status) = Reader::open_unchecked(directory).status()
                && status.state.ended()
            {
                anyhow::bail!(
                    "the session is {} and no longer waits for the Codex composer; no initial console input was sent",
                    status.state
                );
            }
            if composer_is_ready(&last_screen) && clock.now() < deadline {
                return Ok(());
            }
        }
        let remaining = deadline.saturating_duration_since(clock.now());
        clock.sleep(COMPOSER_POLL.min(remaining));
    }
}

fn hook_picker_present(screen: &str) -> bool {
    screen.contains("Review hooks") || screen.contains("Trust all and continue")
}

// Require a known empty composer with no dialog; blank/loading screens prove nothing.
// This is input readiness, not shared consent; see docs/verification/2026-10-02-windows.md.
fn composer_is_ready(screen: &str) -> bool {
    if hook_picker_present(screen) {
        return false;
    }
    legacy_composer_is_ready(screen)
}

// Preserve the pre-record session's readiness predicate as well as its consent OR.
fn legacy_composer_is_ready(screen: &str) -> bool {
    let mut composer = false;
    for line in screen.lines() {
        if line.contains("Trust this folder?") || line.contains("Trust and continue") {
            return false;
        }
        composer |= line.trim() == COMPOSER_ROW;
    }
    composer
}

// Temporary measure for issue #124
fn submission_observed(screen: &str) -> bool {
    let mut composer = false;
    for line in screen.lines().map(str::trim) {
        if line.starts_with("› [Pasted Content") {
            return false;
        }
        composer |= line == COMPOSER_ROW;
    }
    composer
}

fn wait_for_submission_with(
    mut read_screen: impl FnMut() -> Result<String>,
    deadline: Instant,
    poll: Duration,
    mut now: impl FnMut() -> Instant,
) -> terminal::TerminalSendResult {
    while now() < deadline {
        match read_screen() {
            Ok(screen) if submission_observed(&screen) && now() < deadline => return Ok(()),
            Err(_) => break,
            Ok(_) => {}
        }
        thread::sleep(poll.min(deadline.saturating_duration_since(now())));
    }
    Err(terminal::TerminalSendFailure::delivery_uncertain(
        anyhow::anyhow!(
            "Codex composer still holds the pasted prompt; the submission was not observed. Press Enter in the surface or close the session; Agent Bridge does not resend"
        ),
    ))
}

pub(super) fn workspace_trust_override(workspace: &Path) -> Result<String> {
    let mut key = crate::native::consent::native_key(workspace)?;
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    // Codex config/src/overrides.rs splits the left side on every dot and does
    // not parse quoted TOML keys. Put the path in the TOML VALUE instead: quoted
    // dotted keys on the left create a different project and leave the dialog up.
    Ok(format!(
        "projects={{{}={{trust_level=\"trusted\"}}}}",
        serde_json::to_string(&key)?
    ))
}

pub(super) fn codex_launch_arguments(
    bridge_executable: &Path,
    workspace: &Path,
    prompt: &str,
    pending: &PendingCodexTurn,
    windows: bool,
) -> Result<Vec<OsString>> {
    let notify = serde_json::to_string(&[
        bridge_executable.to_string_lossy().as_ref(),
        "native-hook",
        "codex",
    ])?;
    let mut arguments = vec![
        OsString::from("-c"),
        OsString::from(format!("notify={notify}")),
    ];
    if !windows {
        arguments.extend([
            OsString::from("-C"),
            workspace.as_os_str().to_owned(),
            OsString::from(correlated_prompt(prompt, pending)),
        ]);
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests;
