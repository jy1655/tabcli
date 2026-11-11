use super::record::{applied, authorized_with};
use super::store::{Environment, Trust, user_home};
use crate::native::clock::{Clock, SystemClock};
use crate::native::delivery::verify_terminal_surface_ownership_until;
use crate::native::session::{Reader, schema::SessionManifest};
use crate::native::{provider, terminal};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

/// Whether the session was launched under the provider's bypass option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum Bypass {
    On,
    Off,
}

/// The facts of one launch that a provider's launch-dialog policy receives, once.
#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct LaunchFacts<'a> {
    pub(in crate::native) directory: &'a Path,
    pub(in crate::native) workspace: &'a Path,
    pub(in crate::native) bypass: Bypass,
}

/// How a provider adapter treats its provider's startup workspace dialog. Each adapter
/// states its own answer; there is no shared default.
pub(in crate::native) enum LaunchDialogSupport<'a> {
    /// The adapter never answers the dialog, and a launch reads no record for it.
    NotAnswered,
    /// The adapter answers the exact dialog that its policy for the launch recognizes.
    Answered(&'a dyn LaunchDialogAnswerer),
}

/// An adapter's launch-dialog policy, built for one launch from that launch's facts.
pub(in crate::native) trait LaunchDialogAnswerer {
    fn policy<'a>(&'a self, launch: LaunchFacts<'a>) -> Box<dyn LaunchDialogPolicy + 'a>;
}

/// One launch's answers about its provider's startup workspace dialog.
pub(in crate::native) trait LaunchDialogPolicy {
    /// The guarded key that answers this exact screen, or `None` for any other screen. A
    /// policy may recognize a dialog variant only under bypass; it never widens what a
    /// variant asks.
    fn dialog_key(&self, screen: &str) -> Option<terminal::DialogKey>;
    /// How a launch under bypass learns that the provider started without the dialog.
    /// `None` when the bypass option does not change whether the provider shows it.
    fn bypass_start(&self) -> Option<&dyn BypassStart>;
    /// How long a matching screen must stay unchanged before the one response.
    fn settle(&self) -> Duration;
}

/// The provider's own report, read from the session's records, that it started past its
/// launch dialogs.
pub(in crate::native) trait BypassStart {
    fn started(&self) -> Result<ProviderStart>;
    /// The adapter's diagnostic for a launch that reached its deadline unanswered and
    /// unstarted.
    fn not_started(&self) -> String;
}

/// What a provider's start report says about one launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum ProviderStart {
    /// This launch has no start report.
    NoSignal,
    /// The provider has not reported its start yet.
    NotStarted,
    /// The provider reported its start: no dialog is open.
    Started,
}

/// Whether a surface takes a guarded dialog key. A launch asks its surface once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GuardedKey {
    Taken,
    /// The surface takes no guarded key; `surface` names it in the refusal.
    NotTaken {
        surface: &'static str,
    },
}

fn respond_to_workspace_dialog(
    directory: &Path,
    guarded_key: GuardedKey,
    input: &terminal::GuardedDialogInput,
    dispatch: impl FnOnce(&terminal::GuardedDialogInput) -> Result<bool>,
) -> Result<bool> {
    if let GuardedKey::NotTaken { surface } = guarded_key {
        bail!(
            "the workspace trust dialog cannot be answered with a guarded key in {surface}; no key was sent; use the provider's own workspace trust prompt, then launch again"
        );
    }
    applied(directory, "workspace-dialog-response-pending")?;
    dispatch(input)
}

pub(in crate::native) fn complete_launch(
    directory: &Path,
    target: FirstPartyCli,
    session: &terminal::TerminalSession,
    expected_claim: &str,
    deadline: Instant,
) -> Result<()> {
    LaunchDialog {
        directory,
        target,
        expected_claim,
        deadline,
    }
    .complete(
        provider::launch_dialog(target),
        &Environment::current(),
        &mut SystemClock,
        &mut TerminalDialogSurface { directory, session },
    )
}

pub(super) trait DialogSurface {
    fn guarded_key(&self) -> GuardedKey;
    // `session_id` is the manifest's id, which the surface's owner record must name.
    fn screen(&mut self, session_id: &str, deadline: Instant) -> Result<String>;
    fn answer(&mut self, input: &terminal::GuardedDialogInput, deadline: Instant) -> Result<bool>;
}

struct TerminalDialogSurface<'a> {
    directory: &'a Path,
    session: &'a terminal::TerminalSession,
}

impl DialogSurface for TerminalDialogSurface<'_> {
    fn guarded_key(&self) -> GuardedKey {
        if terminal::guards_dialog_input(self.session.kind) {
            GuardedKey::Taken
        } else {
            GuardedKey::NotTaken {
                surface: self.session.kind.display_name(),
            }
        }
    }

    fn screen(&mut self, session_id: &str, deadline: Instant) -> Result<String> {
        verify_terminal_surface_ownership_until(
            self.directory,
            session_id,
            self.session,
            deadline,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        terminal::read_screen(self.session, deadline)
    }

    fn answer(&mut self, input: &terminal::GuardedDialogInput, deadline: Instant) -> Result<bool> {
        terminal::guarded_dialog_input(self.directory, self.session, input, deadline)
    }
}

pub(super) struct LaunchDialog<'a> {
    pub(super) directory: &'a Path,
    pub(super) target: FirstPartyCli,
    pub(super) expected_claim: &'a str,
    pub(super) deadline: Instant,
}

// What the wait for an open dialog works with, decided once before it starts.
struct OpenDialog<'a> {
    manifest: &'a SessionManifest,
    policy: &'a dyn LaunchDialogPolicy,
    // Set only for a launch under bypass whose provider has not reported its start yet.
    bypass_start: Option<&'a dyn BypassStart>,
    guarded_key: GuardedKey,
}

fn require_launch_dialog_live(directory: &Path, responded: bool) -> Result<()> {
    let status = Reader::open_unchecked(directory)
        .status()
        .map_err(|error| launch_dialog_error(error, responded))?;
    if status.state.ended() {
        let detail = if responded {
            "after the one workspace trust response; the provider may have taken it as No"
        } else {
            "before the workspace trust response"
        };
        return Err(launch_dialog_error(
            format!("the session is {} {detail}", status.state),
            responded,
        ));
    }
    Ok(())
}

impl LaunchDialog<'_> {
    /// The launch's workspace dialog step: whether it waits for the provider's dialog at
    /// all, then the wait and the one guarded response.
    pub(super) fn complete(
        &self,
        support: LaunchDialogSupport<'_>,
        env: &Environment,
        clock: &mut impl Clock,
        surface: &mut dyn DialogSurface,
    ) -> Result<()> {
        let LaunchDialogSupport::Answered(answerer) = support else {
            return Ok(());
        };
        let &Self {
            directory, target, ..
        } = self;
        require_launch_dialog_live(directory, false)?;
        if !authorized_with(directory, target, env)? {
            return Ok(());
        }
        let manifest = Reader::open_unchecked(directory).manifest()?;
        let bypass = if manifest.yolo {
            Bypass::On
        } else {
            Bypass::Off
        };
        let policy = answerer.policy(LaunchFacts {
            directory,
            workspace: &manifest.workspace,
            bypass,
        });
        let guarded_key = surface.guarded_key();
        // Under bypass the provider may show no dialog, and then its trust store never
        // changes: only its own start report ends the wait. A launch without that report
        // keeps the dialog unanswered, as before the report existed. A surface that takes
        // no guarded key cannot answer the dialog, and reading it is refused: a launch under
        // bypass there waits for the provider's readiness as it did before the start signal
        // existed, and that wait's diagnostic names the dialog as a possibility.
        let bypass_start = match policy.bypass_start() {
            Some(signal) if bypass == Bypass::On => {
                if guarded_key != GuardedKey::Taken {
                    return Ok(());
                }
                match signal.started()? {
                    ProviderStart::NoSignal | ProviderStart::Started => return Ok(()),
                    ProviderStart::NotStarted => Some(signal),
                }
            }
            _ => None,
        };
        self.wait_and_respond(
            OpenDialog {
                manifest: &manifest,
                policy: policy.as_ref(),
                bypass_start,
                guarded_key,
            },
            env,
            clock,
            surface,
        )
    }

    fn wait_and_respond(
        &self,
        dialog: OpenDialog<'_>,
        env: &Environment,
        clock: &mut impl Clock,
        surface: &mut dyn DialogSurface,
    ) -> Result<()> {
        let &Self {
            directory,
            target,
            expected_claim,
            deadline,
        } = self;
        let OpenDialog {
            manifest,
            policy,
            bypass_start,
            guarded_key,
        } = dialog;
        user_home(env)?;
        let expires_unix_ms = crate::native::session::primitives::unix_ms()
            + deadline.saturating_duration_since(clock.now()).as_millis();
        let settle = policy.settle();
        let mut stable: Option<(String, Instant)> = None;
        let mut responded = false;
        loop {
            require_launch_dialog_live(directory, responded)?;
            let trusted = matches!(
                provider::workspace_trust(target, &manifest.workspace, env, None)
                    .map_err(|error| launch_dialog_error(error, responded))?,
                Trust::Trusted(_)
            );
            require_launch_dialog_live(directory, responded)?;
            if trusted {
                if responded {
                    applied(directory, "provider-workspace-dialog")
                        .context("provider trust is confirmed; recording the Bridge applied marker failed; no input was retried")?;
                }
                return Ok(());
            }
            if clock.now() >= deadline {
                // Under bypass an unanswered wait is not a trust failure: the provider may
                // show no dialog and never report its start. The delivery cannot say so
                // after the deadline, so the adapter's own diagnostic ends the launch here.
                if let Some(signal) = bypass_start
                    && !responded
                {
                    return Err(launch_dialog_error(signal.not_started(), false));
                }
                return Err(launch_dialog_error(
                    "workspace trust was not confirmed before the deadline; inspect workspace_consent and the managed terminal",
                    responded,
                ));
            }
            if !responded {
                // The provider started without the dialog: nothing to answer, and its
                // store will not change. After a response only the store confirms trust.
                if let Some(signal) = bypass_start
                    && signal
                        .started()
                        .map_err(|error| launch_dialog_error(error, false))?
                        == ProviderStart::Started
                {
                    return Ok(());
                }
                if !authorized_with(directory, target, env)
                    .map_err(|error| launch_dialog_error(error, false))?
                {
                    bail!(
                        "workspace consent changed before trust dialog response; no response sent"
                    );
                }
                let screen = surface
                    .screen(&manifest.id, deadline)
                    .map_err(|error| launch_dialog_error(error, false))?;
                let observed_at = clock.now();
                require_launch_dialog_live(directory, false)?;
                if let Some(key) = policy.dialog_key(&screen) {
                    let since = match &stable {
                        Some((previous, since)) if previous == &screen => *since,
                        _ => {
                            stable = Some((screen.clone(), observed_at));
                            observed_at
                        }
                    };
                    if observed_at.saturating_duration_since(since) < settle {
                        let remaining = deadline.saturating_duration_since(clock.now());
                        clock.sleep(Duration::from_millis(200).min(remaining));
                        continue;
                    }
                    if !authorized_with(directory, target, env)
                        .map_err(|error| launch_dialog_error(error, false))?
                    {
                        bail!(
                            "workspace consent changed before trust dialog response; no response sent"
                        );
                    }
                    require_launch_dialog_live(directory, false)?;
                    if clock.now() >= deadline {
                        continue;
                    }
                    let mut attempted = false;
                    responded = respond_to_workspace_dialog(
                        directory,
                        guarded_key,
                        &terminal::GuardedDialogInput {
                            screen,
                            key,
                            expected_claim: Some(expected_claim.to_owned()),
                            expires_unix_ms: Some(expires_unix_ms),
                        },
                        |input| {
                            require_launch_dialog_live(directory, false)?;
                            if clock.now() >= deadline {
                                bail!("workspace trust response deadline expired before dispatch");
                            }
                            attempted = true;
                            surface.answer(input, deadline)
                        },
                    )
                    .map_err(|error| {
                        let attempted = attempted && !error.is::<terminal::StaleDialogClaim>();
                        launch_dialog_error(error, attempted)
                    })?;
                    if !responded {
                        stable = None;
                    }
                    require_launch_dialog_live(directory, responded)?;
                } else {
                    stable = None;
                }
            }
            let remaining = deadline.saturating_duration_since(clock.now());
            clock.sleep(Duration::from_millis(200).min(remaining));
        }
    }
}

fn launch_dialog_error(error: impl std::fmt::Display, responded: bool) -> anyhow::Error {
    let evidence = if responded {
        "possible response; trust unconfirmed; no input was retried"
    } else {
        "no response sent"
    };
    anyhow::anyhow!("{error:#}; {evidence}")
}

#[cfg(test)]
mod tests;
