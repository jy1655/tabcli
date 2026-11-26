//! The one answer to "may this target provider treat this session's workspace as consented
//! now". It derives the answer again from current truth at each use and writes nothing; a
//! session's recheck needs the pins recorded at its start. The session's record says what was
//! assessed at the start and what the launch applied; neither is authority by itself. The yes
//! is consumed at the act, inside the step that performs it: a provider's launch plan and the
//! spawn fence for a launch option, the dispatch of a guarded key, and the dispatch of an Agy
//! paste. See AGENTS.md, Trust Store Boundary.
use super::record::{Assessment, SESSION_FILE, assess_current};
use super::store::{Environment, Trust, identity, user_home};
use crate::native::provider;
use crate::native::session::Reader;
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::str::FromStr;

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
    let reader = Reader::open_unchecked(directory);
    let Some(text) = reader.private(SESSION_FILE).text()? else {
        return Ok(Authority::refused(
            None,
            "the session has no workspace consent record",
        ));
    };
    let assessment: Assessment = serde_json::from_str(&text)?;
    let applied = assessment.applied.clone();
    if assessment.schema != 1 || assessment.state != "verified" {
        return Ok(Authority::refused(
            applied,
            format!("the session's workspace consent is {}", assessment.state),
        ));
    }
    // Bound to the directory the provider runs in.
    let workspace = reader.manifest()?.workspace;
    if workspace != assessment.identity.path || identity(&workspace)? != assessment.identity {
        return Ok(Authority::refused(
            applied,
            "the session's workspace consent names another directory than its workspace",
        ));
    }
    user_home(env)?;
    let root = directory.parent().context("session has no state root")?;
    let now = assess_current(root, &workspace, env)?;
    if now.state != "verified"
        || now.identity != assessment.identity
        || now.source != assessment.source
        || now.repository_root != assessment.repository_root
    {
        return Ok(Authority::refused(
            applied,
            format!(
                "the workspace's consent no longer holds ({}): {}",
                now.state, now.detail
            ),
        ));
    }
    let main = assessment.repository_root.as_ref().map(|root| &root.path);
    for path in std::iter::once(&workspace).chain(main) {
        match provider::workspace_trust(target, path, env, None) {
            Ok(Trust::Declined) => {
                return Ok(Authority::refused(
                    applied,
                    format!("{} declines {}", target.as_str(), path.display()),
                ));
            }
            Err(error) => {
                return Ok(Authority {
                    applied,
                    verdict: Err(Refusal {
                        reason: format!(
                            "{}'s trust of {} cannot be read: {error:#}",
                            target.as_str(),
                            path.display()
                        ),
                        unreadable: true,
                    }),
                });
            }
            Ok(_) => {}
        }
    }
    Ok(Authority {
        applied,
        verdict: Ok(if main.is_some() {
            Consented::Inherited
        } else {
            Consented::Exact
        }),
    })
}

/// The authority as a yes or no, for an adapter that applies consent through its own launch
/// option, with the caller's environment.
pub(in crate::native) fn authorized(directory: &Path, target: FirstPartyCli) -> Result<bool> {
    Ok(authority(directory, target, &Environment::current())?
        .verdict
        .is_ok())
}

/// The spawn fence's check, under the session's lifecycle lock immediately before the provider
/// is started: a trust-bearing launch option that the session's launch applied (its adapter's
/// own marker) still has the authority's yes for the target, which binds it to the workspace
/// the provider starts in. Another adapter's option refuses; a session without a consent record
/// or without an applied option has nothing to check. A record that cannot be read refuses.
pub(in crate::native) fn admit_spawn(directory: &Path) -> Result<()> {
    admit_spawn_with(directory, &Environment::current())
}

pub(super) fn admit_spawn_with(directory: &Path, env: &Environment) -> Result<()> {
    let reader = Reader::open_unchecked(directory);
    let Some(text) = reader.private(SESSION_FILE).text()? else {
        return Ok(());
    };
    let assessment: Assessment = serde_json::from_str(&text)?;
    let Some(applied) = assessment.applied.as_deref() else {
        return Ok(());
    };
    if !provider::launch_trust_markers().any(|marker| marker == applied) {
        return Ok(());
    }
    let target =
        FirstPartyCli::from_str(&reader.manifest()?.provider).map_err(anyhow::Error::msg)?;
    if provider::applied_marker(target) != Some(applied) {
        bail!(
            "the session's consent record names another provider's launch option ({applied}), so the provider was not started"
        );
    }
    let authority = authority(directory, target, env)?;
    match authority.verdict {
        Ok(_) if authority.applied.as_deref() == Some(applied) => Ok(()),
        Ok(_) => bail!("the session's launch option changed, so the provider was not started"),
        Err(refusal) => bail!(
            "workspace consent for the launch option does not hold now, so the provider was not started: {}",
            refusal.reason
        ),
    }
}

#[cfg(test)]
mod tests;
