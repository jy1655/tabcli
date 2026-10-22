use super::delivery::read_log_bytes;
use super::log::{complete_log_lines, glog_timestamp};
use super::{
    ADAPTER, AGY_LOG_FILE, HOOKS_LOADED_SOURCE, STARTUP_POLL_INTERVAL, TRUST_RECOVERY,
    TRUST_STORE_MISSING,
};
use crate::native::clock::Clock;
use crate::native::consent::{self, TrustReadiness};
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::Reader;
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

// Require exact stored trust and this process's non-main hooks load; shared trust alone is insufficient.
// A missing line does not prove a dialog; see docs/verification/2026-10-01-bug-triage.md.

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

// Missing or unreadable trust evidence keeps waiting; timeout or session end returns not_sent.
// Never paste into an ended session or infer consent from a failed lookup. The loop is
// consent's shared trust wait; this is Agy's readiness and its two messages. Tests call
// this seam with a simulated clock and their own observations.
fn wait_for_workspace_trust_evidence<C: Clock>(
    directory: &Path,
    deadline: Instant,
    clock: &mut C,
    missing: &mut dyn FnMut() -> Result<Option<&'static str>>,
) -> Result<()> {
    consent::wait_until_trusted(
        directory,
        deadline,
        consent::TrustPoll {
            interval: STARTUP_POLL_INTERVAL,
            sleep_bound: consent::SleepBound::CapturedNow,
        },
        clock,
        &|state| {
            format!(
                "the session is {state} and no longer waits for Agy workspace trust, so the prompt was not pasted"
            )
        },
        &mut || {
            Ok(match missing() {
                Ok(None) => TrustReadiness::Ready,
                Ok(Some(missing)) => TrustReadiness::Pending(missing.to_owned()),
                Err(error) => TrustReadiness::Pending(format!(
                    "Agy's trust evidence could not be read ({error:#})"
                )),
            })
        },
        &|reason| {
            format!(
                "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {reason}. {TRUST_RECOVERY}"
            )
        },
    )
}

pub(super) fn workspace_trust_missing(
    workspace: &Path,
    env: &consent::Environment,
    log_path: &Path,
) -> Result<Option<&'static str>> {
    use consent::Trust;
    if !matches!(
        ADAPTER.workspace_trust(workspace, env, None)?,
        Trust::Trusted(_)
    ) {
        return Ok(Some(TRUST_STORE_MISSING));
    }
    let loaded = read_log_bytes(log_path)?
        .as_deref()
        .is_some_and(workspace_customizations_loaded);
    Ok((!loaded).then_some(TRUST_SESSION_UNVERIFIED))
}

// The gate takes the caller's environment once per delivery; the user home is required
// after the manifest, as before.
pub(super) fn wait_for_workspace_trust<C: Clock>(
    directory: &Path,
    deadline: Instant,
    env: &consent::Environment,
    clock: &mut C,
) -> Result<()> {
    let workspace = Reader::open_unchecked(directory).manifest()?.workspace;
    consent::user_home(env)?;
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    wait_for_workspace_trust_evidence(directory, deadline, clock, &mut || {
        workspace_trust_missing(&workspace, env, &log_path)
    })
}

#[cfg(test)]
mod tests;
