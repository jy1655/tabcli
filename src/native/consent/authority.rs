//! The one answer to "may this target provider treat this session's workspace as consented
//! now". It derives the answer again from current truth at each use and writes nothing; a
//! session's recheck needs the pins recorded at its start. The session's record says what was
//! assessed at the start and what the launch applied; neither is authority by itself. The yes
//! is consumed at the act, inside the step that performs it: a provider's launch plan and the
//! spawn fence for a launch option, the dispatch of a guarded key, and the dispatch of an Agy
//! paste. A trust-bearing launch option is asked about as the command that will start, carried
//! from its plan (`LaunchGrant`). See AGENTS.md, Trust Store Boundary.
use super::record::{Assessment, SESSION_FILE, assess_current};
use super::store::{Environment, Identity, Trust, identity, user_home};
use crate::native::provider;
use crate::native::session::Reader;
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail, ensure};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

/// How a session's workspace is consented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Consented {
    /// Exact trust of the workspace in a provider's own store.
    Exact,
    /// The consent of the main working tree of the repository that registered the workspace
    /// as a linked git worktree.
    Inherited,
}

/// What the authority found for one target provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) struct Authority {
    /// The launch option that the session's record says its launch applied: configuration,
    /// never authority.
    pub(in crate::native) applied: Option<String>,
    /// Consented now, and how; or why not.
    pub(in crate::native) verdict: std::result::Result<Consented, Refusal>,
}

/// Why the authority refused: a fact that does not hold, or evidence that cannot be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) struct Refusal {
    pub(in crate::native) reason: String,
    pub(in crate::native) unreadable: bool,
}

impl Authority {
    fn refused(applied: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            applied,
            verdict: Err(Refusal {
                reason: reason.into(),
                unreadable: false,
            }),
        }
    }
}

/// Whether `target` may treat the session's workspace as consented now. It holds only when
/// all of these hold now: the session's record is a verified assessment of the manifest's
/// workspace, by path and identity; a fresh assessment of that workspace (without writing)
/// finds the same source, and for inherited consent the same main working tree, whose own
/// consent is assessed as TAB assesses an exact folder (its pinned source, identity and
/// revocation) while Git's registration still verifies and the source provider does not
/// decline the worktree; and the target provider neither declines nor fails to read the
/// workspace or that main working tree. A record or manifest that cannot be read is an error.
pub(in crate::native) fn authority(
    directory: &Path,
    target: FirstPartyCli,
    env: &Environment,
) -> Result<Authority> {
    Ok(authority_for(directory, target, env)?.0)
}

// The authority, and the identity of the workspace it found consented (none with a refusal).
fn authority_for(
    directory: &Path,
    target: FirstPartyCli,
    env: &Environment,
) -> Result<(Authority, Option<Identity>)> {
    let reader = Reader::open_unchecked(directory);
    let Some(text) = reader.private(SESSION_FILE).text()? else {
        return Ok((
            Authority::refused(None, "the session has no workspace consent record"),
            None,
        ));
    };
    let assessment: Assessment = serde_json::from_str(&text)?;
    let applied = assessment.applied.clone();
    if assessment.schema != 1 || assessment.state != "verified" {
        return Ok((
            Authority::refused(
                applied,
                format!("the session's workspace consent is {}", assessment.state),
            ),
            None,
        ));
    }
    // Bound to the directory the provider runs in.
    let workspace = reader.manifest()?.workspace;
    if workspace != assessment.identity.path || identity(&workspace)? != assessment.identity {
        return Ok((
            Authority::refused(
                applied,
                "the session's workspace consent names another directory than its workspace",
            ),
            None,
        ));
    }
    user_home(env)?;
    let root = directory.parent().context("session has no state root")?;
    // Evidence that the recheck cannot read is a refusal marked unreadable (review r4, finding
    // 1): a consumer that does not need the cause plans no option, sends no key and still tries
    // its independent routes, as a missing consent did in 0.4.49; Agy reports the cause, and a
    // granted command refuses to start.
    let now = match assess_current(root, &workspace, env) {
        Ok(now) => now,
        Err(error) => {
            return Ok((
                Authority {
                    applied,
                    verdict: Err(Refusal {
                        reason: format!("the workspace's consent cannot be read now: {error:#}"),
                        unreadable: true,
                    }),
                },
                None,
            ));
        }
    };
    if now.state != "verified"
        || now.identity != assessment.identity
        || now.source != assessment.source
        || now.repository_root != assessment.repository_root
    {
        return Ok((
            Authority::refused(
                applied,
                format!(
                    "the workspace's consent no longer holds ({}): {}",
                    now.state, now.detail
                ),
            ),
            None,
        ));
    }
    let main = assessment.repository_root.as_ref().map(|root| &root.path);
    for path in std::iter::once(&workspace).chain(main) {
        match provider::workspace_trust(target, path, env, None) {
            Ok(Trust::Declined) => {
                return Ok((
                    Authority::refused(
                        applied,
                        format!("{} declines {}", target.as_str(), path.display()),
                    ),
                    None,
                ));
            }
            Err(error) => {
                return Ok((
                    Authority {
                        applied,
                        verdict: Err(Refusal {
                            reason: format!(
                                "{}'s trust of {} cannot be read: {error:#}",
                                target.as_str(),
                                path.display()
                            ),
                            unreadable: true,
                        }),
                    },
                    None,
                ));
            }
            Ok(_) => {}
        }
    }
    Ok((
        Authority {
            applied,
            verdict: Ok(if main.is_some() {
                Consented::Inherited
            } else {
                Consented::Exact
            }),
        },
        Some(assessment.identity),
    ))
}

/// The authority as a yes or no, for an adapter that applies consent through its own launch
/// option, with the caller's environment.
pub(in crate::native) fn authorized(directory: &Path, target: FirstPartyCli) -> Result<bool> {
    Ok(authority(directory, target, &Environment::current())?
        .verdict
        .is_ok())
}

/// A trust-bearing launch option that a launch plan put in its provider's command: the target
/// provider, the option's marker and the identity of the workspace whose consent granted it.
/// The plan carries it to the spawn fence, which asks the authority again about exactly this
/// command (review r3, finding 1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) struct LaunchGrant {
    pub(in crate::native) provider: FirstPartyCli,
    pub(in crate::native) marker: &'static str,
    pub(in crate::native) workspace: Identity,
}

/// The spawn fence's check for a command that carries a trust-bearing launch option, under the
/// session's lifecycle lock immediately before the fence's deadline check: the session's
/// provider is the option's, the authority still says yes for that provider, the session's
/// record still names this option, and the consented workspace is the one the plan was granted
/// for. `budget` is the launch's remaining time: an answer that does not come within it refuses,
/// so the assessment cannot push a start past the receipt's expiry. A command without such an
/// option is never asked about (0.4.49's admission). A refusal starts nothing.
pub(in crate::native) fn admit_launch(
    directory: &Path,
    grant: &LaunchGrant,
    budget: Option<Duration>,
) -> Result<()> {
    admit_launch_within(directory, grant, budget, Environment::current())
}

pub(super) fn admit_launch_within(
    directory: &Path,
    grant: &LaunchGrant,
    budget: Option<Duration>,
    env: Environment,
) -> Result<()> {
    let Some(budget) = budget else {
        return admit_launch_with(directory, grant, &env);
    };
    let (owned, owned_grant) = (directory.to_owned(), grant.clone());
    within(budget, move || {
        admit_launch_with(&owned, &owned_grant, &env)
    })
}

// `check` answered within `budget` (review r4, finding 3). No time left refuses without asking.
// Otherwise one absolute monotonic deadline is taken before the check starts, and an answer
// that has not arrived by it, or arrives after it, refuses. The check writes nothing, so a late
// answer is simply dropped.
fn within(budget: Duration, check: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    const LATE: &str = "workspace consent for the launch option could not be confirmed within the launch's remaining time, so the provider was not started";
    if budget.is_zero() {
        bail!(LATE);
    }
    let deadline = std::time::Instant::now() + budget;
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(check());
    });
    let answer = received
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .map_err(|_| anyhow::anyhow!(LATE))?;
    ensure!(std::time::Instant::now() <= deadline, LATE);
    answer
}

pub(super) fn admit_launch_with(
    directory: &Path,
    grant: &LaunchGrant,
    env: &Environment,
) -> Result<()> {
    let provider = FirstPartyCli::from_str(&Reader::open_unchecked(directory).manifest()?.provider)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        provider == grant.provider,
        "the launch option belongs to another provider than the session's, so the provider was not started"
    );
    let (found, consented) = authority_for(directory, grant.provider, env)?;
    match found.verdict {
        Ok(_)
            if found.applied.as_deref() == Some(grant.marker)
                && consented.as_ref() == Some(&grant.workspace) =>
        {
            Ok(())
        }
        Ok(_) => bail!(
            "the session's consent no longer names this command's launch option and workspace, so the provider was not started"
        ),
        Err(refusal) => bail!(
            "workspace consent for the launch option does not hold now, so the provider was not started: {}",
            refusal.reason
        ),
    }
}

#[cfg(test)]
mod tests;
