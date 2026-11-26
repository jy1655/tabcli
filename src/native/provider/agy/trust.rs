use super::delivery::read_log_bytes;
use super::log::{complete_log_lines, glog_timestamp};
use super::{
    ADAPTER, AGY_LOG_FILE, APPLIED_MARKER, HOOKS_LOADED_SOURCE, STARTUP_POLL_INTERVAL,
    TRUST_RECOVERY, TRUST_STORE_MISSING,
};
use crate::native::clock::Clock;
use crate::native::consent::{self, TrustReadiness};
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::Reader;
use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

// Require exact stored trust, or this session's own session-only setting while consent still
// holds, and this process's non-main hooks load; shared trust alone is insufficient, and so is
// the setting alone. A missing line does not prove a dialog; see
// docs/verification/2026-10-01-bug-triage.md.

const TRUST_SESSION_UNVERIFIED: &str = "Agy's trust store lists the workspace, but this session's agy.log does not show that this session loaded the workspace customizations (no `hooks_manager.go` line outside the main goroutine), so its own trust dialog may still be open";

pub(super) const TRUST_SETTING_UNVERIFIED: &str = "This session's launch configured Agy's session-only setting that trusts the exact workspace, but this session's own log does not show that it loaded the workspace customizations (no `hooks_manager.go` line outside the main goroutine), so Agy may not have started or taken the setting, and its trust dialog may be open";

pub(super) const TRUST_SETTING_WITHOUT_CONSENT: &str = "This session's launch configured Agy's session-only setting that trusts the exact workspace, but the workspace's consent does not hold now (it was revoked or reset, its source no longer trusts it, Agy declines it or its trust cannot be read, or it is no longer a verified worktree of a trusted repository)";

// Which first trust fact holds for a paste.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TrustRoute {
    // Agy's own store lists the exact workspace.
    Store,
    // This session's launch configured Agy's session-only setting for the workspace, and
    // consent's authority finds the workspace consented now.
    Setting,
}

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

// The first trust fact is that Agy was told to trust the exact workspace: its own store lists
// it, or this session's launch handed it over in Agy's session-only settings (then the shared
// store is not read). The second is always this process's own log line.
// Both trust facts of a paste, or the first one that is missing. The first fact is Agy's own
// store entry, read first as every earlier release did; only a session whose record says its
// launch applied the setting is asked about the setting, and then consent's authority decides
// from its own reading of that record: the marker alone is never enough. The second fact is
// always this process's own log line.
pub(super) fn workspace_trust_evidence(
    workspace: &Path,
    env: &consent::Environment,
    directory: &Path,
) -> Result<std::result::Result<TrustRoute, &'static str>> {
    use consent::Trust;
    let route = match ADAPTER.workspace_trust(workspace, env, None) {
        Ok(Trust::Trusted(_)) => TrustRoute::Store,
        store => {
            if consent::observe(directory).applied_marker() != Some(APPLIED_MARKER) {
                store?;
                return Ok(Err(TRUST_STORE_MISSING));
            }
            let authority = consent::authority(directory, FirstPartyCli::Agy, env)?;
            if authority.applied.as_deref() != Some(APPLIED_MARKER) || authority.verdict.is_err() {
                return Ok(Err(TRUST_SETTING_WITHOUT_CONSENT));
            }
            TrustRoute::Setting
        }
    };
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let loaded = read_log_bytes(&log_path)?
        .as_deref()
        .is_some_and(workspace_customizations_loaded);
    Ok(if loaded {
        Ok(route)
    } else {
        Err(match route {
            TrustRoute::Store => TRUST_SESSION_UNVERIFIED,
            TrustRoute::Setting => TRUST_SETTING_UNVERIFIED,
        })
    })
}

// The missing fact, or `None` when the paste may go ahead.
pub(super) fn workspace_trust_missing(
    workspace: &Path,
    env: &consent::Environment,
    directory: &Path,
) -> Result<Option<&'static str>> {
    Ok(workspace_trust_evidence(workspace, env, directory)?.err())
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
    wait_for_workspace_trust_evidence(directory, deadline, clock, &mut || {
        workspace_trust_missing(&workspace, env, directory)
    })
}

#[cfg(test)]
mod tests;
