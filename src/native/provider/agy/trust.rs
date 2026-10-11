use super::delivery::{Clock, SystemClock, complete_log_lines, glog_timestamp, read_log_bytes};
use super::{
    ADAPTER, AGY_LOG_FILE, HOOKS_LOADED_SOURCE, STARTUP_POLL_INTERVAL, TRUST_RECOVERY,
    TRUST_STORE_MISSING,
};
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::{Reader, SessionState};
use anyhow::{Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

// The trust evidence a paste needs (issue #48): Agy's trust store lists the exact
// workspace, and this session's own log shows that this Agy process loaded the
// workspace customizations. The store alone is shared by every Agy process: a
// dialog approved in one session leaves the dialog of another session of the same
// workspace open (session-UuYk87, 2026-10-01: store entry present, dialog still on
// screen for five seconds until it was approved there too). Agy loads the
// customizations only once it trusts the workspace, at startup when the store
// already lists it or when the dialog is approved in that process (`AddWorkspaceDir`
// -> `reloadWorkspaceCustomizations` -> `ReloadHooks`), and either way a goroutine
// other than the main one logs a `hooks_manager.go` line; the main goroutine logs
// the only other one while the store manager is built. In 112 logs of Agy 1.2.6 to
// 1.2.14 on macOS and in the Windows fixtures of the tests, every delivered paste
// follows such a line and no session with its dialog still open has one.
//
// A log without the line withholds the paste but does not prove an open dialog: the
// log can be missing, or cut before the line, in a session that has none. The
// reports therefore state what was read, and the recovery is conditional on what the
// managed terminal shows.

const TRUST_SESSION_UNVERIFIED: &str = "Agy's trust store lists the workspace, but this session's agy.log does not show that this session loaded the workspace customizations (no `hooks_manager.go` line outside the main goroutine), so its own trust dialog may still be open";

pub(super) fn workspace_customizations_loaded(log: &[u8]) -> bool {
    complete_log_lines(log).any(|line| {
        glog_timestamp(&line).is_some()
            && line
                .get(21..)
                .and_then(|rest| rest.trim_start().split_once(' '))
                .is_some_and(|(goroutine, rest)| {
                    goroutine != "1"
                        && !goroutine.is_empty()
                        && goroutine.bytes().all(|byte| byte.is_ascii_digit())
                        && rest.starts_with(HOOKS_LOADED_SOURCE)
                })
    })
}

// Waits until `missing` reports nothing missing. An absent entry, a session whose
// log lacks the load, an unreadable store or log, and a lookup error all keep
// waiting, since none proves the dialog is gone: a human approves it in the managed
// terminal, or shared consent answered it at launch. The deadline ends the wait as
// `not_sent`, before any terminal input, and so does a session that has ended
// (`ended` names its state): it has no terminal left to paste into, and the launcher
// of a closed session went on waiting until its deadline (issue #60).
fn wait_for_workspace_trust_with<T, C>(
    missing: &mut T,
    ended: &mut dyn FnMut() -> Option<String>,
    deadline: Instant,
    poll_interval: Duration,
    clock: &mut C,
) -> Result<()>
where
    T: FnMut() -> Result<Option<&'static str>>,
    C: Clock,
{
    loop {
        if let Some(state) = ended() {
            bail!(
                "the session is {state} and no longer waits for Agy workspace trust, so the prompt was not pasted"
            );
        }
        let observed = missing();
        if matches!(observed, Ok(None)) {
            return Ok(());
        }
        let now = clock.now();
        if now >= deadline {
            let reason = match observed {
                Err(error) => format!("Agy's trust evidence could not be read ({error:#})"),
                Ok(missing) => missing.unwrap_or_default().to_owned(),
            };
            bail!(
                "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {reason}. {TRUST_RECOVERY}"
            );
        }
        clock.sleep(deadline.saturating_duration_since(now).min(poll_interval));
    }
}

pub(super) fn workspace_trust_missing(
    workspace: &Path,
    homes: &super::super::super::consent::Homes,
    log_path: &Path,
) -> Result<Option<&'static str>> {
    use super::super::super::consent::Trust;
    if !matches!(
        ADAPTER.workspace_trust(workspace, homes)?,
        Trust::Trusted(_)
    ) {
        return Ok(Some(TRUST_STORE_MISSING));
    }
    let loaded = read_log_bytes(log_path)?
        .as_deref()
        .is_some_and(workspace_customizations_loaded);
    Ok((!loaded).then_some(TRUST_SESSION_UNVERIFIED))
}

pub(super) fn wait_for_workspace_trust(directory: &Path, deadline: Instant) -> Result<()> {
    let workspace = Reader::open_unchecked(directory).manifest()?.workspace;
    let homes = super::super::super::consent::Homes::current()?;
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    wait_for_workspace_trust_with(
        &mut || workspace_trust_missing(&workspace, &homes, &log_path),
        &mut || {
            Reader::open_unchecked(directory)
                .status()
                .ok()
                .map(|status| status.state)
                .filter(|state| {
                    matches!(
                        state,
                        SessionState::Failed | SessionState::Exited | SessionState::Closed
                    )
                })
                .map(|state| state.to_string())
        },
        deadline,
        STARTUP_POLL_INTERVAL,
        &mut SystemClock,
    )
}

#[cfg(test)]
mod tests;
