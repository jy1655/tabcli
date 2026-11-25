//! Close authority: one decision over one observation seam, dispatched to the actions.
//!
//! The decision consults nothing but `CloseObservation`: the session's Records (read
//! when the decision asks, in the order it asks) and the OS and surface observations.
//! `Production` is the adapter that reads the session directory and observes the system;
//! tests use the in-memory adapter in `test_support`. The actions stay in `ownership`
//! (Terminal.app owner teardown) and the platform adapters (surface close).
use super as terminal;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::CloseOutcome;
use super::TerminalSession;
use super::ownership::verify_terminal_surface_ownership_with_timeout;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use super::ownership::{NativeSessionOwner, OwnerBinding};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::ownership::{
    owner_group_gone, teardown_absent_owner_with, terminate_owned_foreground_group_until,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::posix::OwnedSurfaceClose;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::posix_ownership::verified_posix_terminal_owner;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::process_identity::ProcessObservation;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::SessionStatus;
use crate::native::session::close::budget::close_remaining;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::owner::{OwnerAttestation, OwnerRecord, attest_owner};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::session::{self, Store};
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::{Reader, SessionState, launch};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use agent_bridge::process_is_alive;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::{Result, bail};
use std::{path::Path, time::Instant};

pub(in crate::native) mod owner_exit;
#[cfg(test)]
pub(in crate::native) mod test_support;

#[derive(Debug, Eq, PartialEq)]
pub(in crate::native) enum TerminalCloseAuthority {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    LiveOwner,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    OwnerTeardown,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    TeardownComplete(String),
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    SurfaceOnly,
    /// The kind's core keeps its terminals and no live owner remains to close with: its owner
    /// is verified gone (exited or reused, whatever its record says) or, for a failed start,
    /// was never recorded. The adapter removes the terminal only after proving from the
    /// creation record in the handle that it is the one this session created, and ends a
    /// running one only as `Running` allows. The owner record grants nothing here.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    CreatedSurface(Running),
    Absent,
}

/// Whether a close on a terminal's creation record may end it while it runs. `End` holds only
/// when this session's owner is positively gone (a record bound to this session, naming an
/// incarnation that was observed exited or reused) or a failed start recorded no owner; even
/// then the adapter ends the terminal only after observing its program with the recorded
/// birth. `Keep` removes a finished terminal and settles an absent one, and retains a running
/// one: the owner record cannot show that this session has ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
pub(in crate::native) enum Running {
    End,
    Keep,
}

/// Why a close runs. Only `close-session --explicit` (and the sessions it lists with
/// `--with-started-from`) may act on a gone owner's terminal by its creation record; a close
/// that cleans up after another command keeps the earlier authorities only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum CloseCause {
    Explicit,
    Cleanup,
}

// Everything the decision consults. The Record accessors read when they are called, so
// the decision's call order is the Record-read order; the observations are the distinct
// OS and surface proofs that the production adapter keeps apart.
pub(in crate::native) trait CloseObservation {
    /// Why the close runs, as its caller states it.
    fn cause(&self) -> CloseCause {
        CloseCause::Explicit
    }
    /// Some only for a kind declaring BridgeCreatedHost: this observation proves the
    /// whole recorded host incarnation, independent of owner or intent. Unknown refuses.
    /// It never treats a missing endpoint or pane as an absent host.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    fn created_host_incarnation(
        &self,
    ) -> Result<Option<crate::native::process_identity::ProcessObservation>> {
        Ok(None)
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn teardown_intent(&self) -> Result<Option<session::close::TerminalTeardownIntent>>;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn owner_present(&self) -> Result<bool>;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn launch(&self) -> Result<Option<launch::Record>>;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn status(&self) -> Result<SessionStatus>;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn owner(&self) -> Result<NativeSessionOwner>;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn resumable(&self) -> Result<bool>;
    fn surface_present(&self) -> Result<bool>;
    fn ownership_proof(&self) -> Result<()>;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn observe_owner_record(&self, owner: &NativeSessionOwner) -> OwnerAttestation;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn pid_alive(&self, pid: u32) -> bool;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn posix_owner_live(&self, owner: &NativeSessionOwner) -> OwnerAttestation;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn attest_owner(&self) -> Result<()>;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn group_gone(&self, group: u32) -> Result<bool>;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn evidence(&self) -> &dyn terminal::close_evidence::CloseEvidence;
}

// The production adapter: the session directory, read at each call, and the system.
// It holds no Record and takes no lock; the closer constructs it after the handle claim.
pub(in crate::native) struct Production<'a> {
    directory: &'a Path,
    expected_session_id: &'a str,
    session: &'a TerminalSession,
    deadline: Instant,
    cause: CloseCause,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    posix_adapters: &'a dyn terminal::posix::Adapters,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    observe_owner: &'a dyn Fn(&NativeSessionOwner) -> OwnerAttestation,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pid_is_alive: &'a dyn Fn(u32) -> bool,
    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    scripted_dead_owner: bool,
}

impl<'a> Production<'a> {
    pub(in crate::native) fn new(
        directory: &'a Path,
        expected_session_id: &'a str,
        session: &'a TerminalSession,
        deadline: Instant,
    ) -> Self {
        Self {
            directory,
            expected_session_id,
            session,
            deadline,
            cause: CloseCause::Explicit,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            posix_adapters: terminal::posix_installed(),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            observe_owner: &attest_owner,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            pid_is_alive: &process_is_alive,
            #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
            scripted_dead_owner: false,
        }
    }

    pub(in crate::native) fn for_cause(mut self, cause: CloseCause) -> Self {
        self.cause = cause;
        self
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    pub(in crate::native::terminal) fn with_posix_dead_owner_observations(
        mut self,
        adapters: &'a dyn terminal::posix::Adapters,
        observe: &'a dyn Fn(&NativeSessionOwner) -> OwnerAttestation,
        pid_is_alive: &'a dyn Fn(u32) -> bool,
    ) -> Self {
        // Herdr's presence evidence does not probe an app or host incarnation.
        assert_eq!(self.session.kind, terminal::TerminalKind::Herdr);
        self.posix_adapters = adapters;
        self.observe_owner = observe;
        self.pid_is_alive = pid_is_alive;
        self.scripted_dead_owner = true;
        self
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    fn require_installed_observation(&self, operation: &str) {
        assert!(
            !self.scripted_dead_owner,
            "scripted Herdr dead-owner Close cannot {operation}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn reader(&self) -> Reader {
        Reader::open_unchecked(self.directory)
    }
}

impl CloseObservation for Production<'_> {
    fn cause(&self) -> CloseCause {
        self.cause
    }

    fn created_host_incarnation(
        &self,
    ) -> Result<Option<crate::native::process_identity::ProcessObservation>> {
        terminal::close_evidence::created_host_incarnation(self.expected_session_id, self.session)
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn teardown_intent(&self) -> Result<Option<session::close::TerminalTeardownIntent>> {
        session::close::terminal_teardown_intent(&self.reader(), self.session)
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn owner_present(&self) -> Result<bool> {
        // A record that cannot be read is refused here, before anything else is read; a
        // malformed one is present and the owner read reports it.
        Ok(
            match crate::native::session::owner::observe(&self.reader()) {
                OwnerRecord::Absent => false,
                OwnerRecord::Unreadable(error) => return Err(error),
                OwnerRecord::Malformed { .. } | OwnerRecord::Recorded(_) => true,
            },
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn launch(&self) -> Result<Option<launch::Record>> {
        launch::read(&self.reader())
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn status(&self) -> Result<SessionStatus> {
        self.reader().status()
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn owner(&self) -> Result<NativeSessionOwner> {
        self.reader().owner()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn resumable(&self) -> Result<bool> {
        #[cfg(test)]
        if self.scripted_dead_owner {
            // Keep the Record reads. Refuse before an exact intent can inspect a real owner.
            return session::close::terminal_close_resumable_with(
                &self.reader(),
                self.session,
                terminal::surface_outlives_owner(self.session),
                |_| panic!("scripted Herdr dead-owner Close cannot attest a resumable Close"),
            );
        }
        session::close::terminal_close_resumable(
            &self.reader(),
            self.session,
            terminal::surface_outlives_owner(self.session),
        )
    }

    fn surface_present(&self) -> Result<bool> {
        let remaining = close_remaining(self.deadline)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        return terminal::posix::surface_present(
            self.posix_adapters,
            self.directory,
            self.session,
            remaining,
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        terminal::surface_present(self.directory, self.session, remaining)
    }

    fn ownership_proof(&self) -> Result<()> {
        #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
        self.require_installed_observation("verify live ownership");
        verify_terminal_surface_ownership_with_timeout(
            self.directory,
            self.expected_session_id,
            self.session,
            Some(close_remaining(self.deadline)?),
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn observe_owner_record(&self, owner: &NativeSessionOwner) -> OwnerAttestation {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        return (self.observe_owner)(owner);
        #[cfg(windows)]
        attest_owner(owner)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn pid_alive(&self, pid: u32) -> bool {
        (self.pid_is_alive)(pid)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn posix_owner_live(&self, owner: &NativeSessionOwner) -> OwnerAttestation {
        (self.observe_owner)(owner)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn attest_owner(&self) -> Result<()> {
        #[cfg(test)]
        self.require_installed_observation("verify the live owner");
        verified_posix_terminal_owner(self.directory, self.expected_session_id, self.session, None)
            .map(|_| ())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn group_gone(&self, group: u32) -> Result<bool> {
        #[cfg(test)]
        self.require_installed_observation("probe the owner's process group");
        owner_group_gone(group)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn evidence(&self) -> &dyn terminal::close_evidence::CloseEvidence {
        terminal::close_evidence(self.session.kind)
    }
}

// The decision. Its Record reads and their order are part of its contract: teardown
// intent first, owner presence by text, the launch record before the failed-start tests,
// then status and owner in that branch, resumability as a fresh call, the owner again
// before attestation, and status only in the no-owner branch.
pub(in crate::native) fn decide(
    observation: &dyn CloseObservation,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    deadline: std::time::Instant,
) -> Result<TerminalCloseAuthority> {
    close_remaining(deadline)?;
    let surface_present = || {
        close_remaining(deadline)?;
        let result = observation.surface_present();
        close_remaining(deadline)?;
        result
    };
    let ownership_proof = || {
        close_remaining(deadline)?;
        let result = observation.ownership_proof();
        close_remaining(deadline)?;
        result
    };
    let _ = (expected_session_id, session, &surface_present);
    // This is independent close authority, not resumability: missing/mismatched
    // owner intents cannot weaken the creation-proven whole-host identity. A foreign
    // handle is still refused before observing any host. No current macOS kind selects it.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if let Some(host) = observation.created_host_incarnation()? {
        close_remaining(deadline)?;
        session.verify_managed_session(expected_session_id)?;
        return match host {
            ProcessObservation::Live(crate::native::process_identity::Attested::Incarnation) => {
                Ok(TerminalCloseAuthority::SurfaceOnly)
            }
            ProcessObservation::Gone(_) => Ok(TerminalCloseAuthority::Absent),
            ProcessObservation::Unknown(reason) => {
                bail!("cannot verify TAB-created host: {reason}")
            }
            ProcessObservation::Live(crate::native::process_identity::Attested::PidOnly) => {
                bail!("TAB-created host requires an incarnation")
            }
        };
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let teardown_intent = observation.teardown_intent()?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let has_teardown_intent = teardown_intent.is_some();
        #[cfg(windows)]
        let has_teardown_intent = false;
        if observation.owner_present()? {
            // An explicit close may reclaim the exact startup surface after its wrapper
            // died, including an uncertain spawn. This is not automatic failure cleanup:
            // neither a live/unknown owner nor an unbound/foreign handle gains authority.
            // Terminal.app, Warp and WezTerm gain none here: a failed launch is no close
            // intent, and the dead-owner rules below apply to them.
            if let Some(launch) = observation.launch()?
                && !has_teardown_intent
                && launch.phase != launch::Phase::Spawned
                && terminal::facts(session.kind).failed_start_identity
            {
                let status: SessionStatus = observation.status()?;
                let owner: NativeSessionOwner = observation.owner()?;
                let observed = observation.observe_owner_record(&owner);
                if status.state == SessionState::Failed
                    && owner.binding(expected_session_id) == OwnerBinding::Bound
                    && matches!(observed.process, ProcessObservation::Gone(_))
                {
                    session.verify_managed_session(expected_session_id)?;
                    return Ok(owner_exit::startup_surface(
                        session,
                        owner_exit::this_sessions_owner_gone(
                            &owner,
                            expected_session_id,
                            &observed.process,
                        ),
                    ));
                }
            }
            // The close that recorded the intent may finish without signalling anything
            // again, and only once the incarnation it verified no longer exists. A live
            // or uninspectable incarnation keeps the rules below; a reused PID alone
            // grants nothing outside this exact-intent route. A kind whose core keeps its
            // terminals needs no intent: a gone owner's close proves the terminal by its
            // creation record on every OS (`owner_exit`).
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if !has_teardown_intent
                && !owner_exit::core_retains_finished(session.kind)
                && observation.resumable()?
            {
                session.verify_managed_session(expected_session_id)?;
                return Ok(
                    match observation
                        .evidence()
                        .resumable_authority(&|| observation.owner(), Box::new(surface_present))?
                    {
                        terminal::close_evidence::ResumableAuthority::Absent => {
                            TerminalCloseAuthority::Absent
                        }
                        terminal::close_evidence::ResumableAuthority::SurfaceOnly => {
                            TerminalCloseAuthority::SurfaceOnly
                        }
                    },
                );
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                let owner: NativeSessionOwner = observation.owner()?;
                let attestation = observation.posix_owner_live(&owner);
                if let ProcessObservation::Unknown(reason) = &attestation.process {
                    bail!("{reason}");
                }
                match &attestation.control {
                    crate::native::session::owner::ControlAttestation::Changed(reason)
                    | crate::native::session::owner::ControlAttestation::Unknown(reason) => {
                        bail!("{reason}")
                    }
                    _ => {}
                }
                if matches!(attestation.process, ProcessObservation::Gone(_)) {
                    return owner_exit::explicit_close(
                        observation,
                        expected_session_id,
                        session,
                        &owner,
                        attestation.process,
                        teardown_intent,
                        surface_present,
                    );
                }
                // Keep the existing app-incarnation guard as part of the live proof.
                // Only its ended-incarnation refusal moves to the absent-surface row.
                let ended = observation.evidence().incarnation_ended(&owner)?;
                let proof = if ended {
                    Err(anyhow::anyhow!(
                        "Terminal.app ended while its native owner remained; no close was sent"
                    ))
                } else {
                    ownership_proof()
                };
                if let Err(error) = proof {
                    close_remaining(deadline)?;
                    let absent = observation
                        .evidence()
                        .surface_absent(Some(&owner), Box::new(surface_present));
                    if !matches!(absent, Ok(true)) {
                        return Err(error);
                    }
                    observation.attest_owner()?;
                    return Ok(TerminalCloseAuthority::OwnerTeardown);
                }
                return Ok(TerminalCloseAuthority::LiveOwner);
            }
            #[cfg(windows)]
            {
                if let Some(authority) =
                    owner_exit::windows_explicit_close(observation, expected_session_id, session)?
                {
                    return Ok(authority);
                }
                ownership_proof()?;
                return Ok(TerminalCloseAuthority::LiveOwner);
            }
        }
        let status: SessionStatus = observation.status()?;
        if !matches!(status.state, SessionState::Launching | SessionState::Failed) {
            bail!(
                "terminal close requires a live native-session owner while the session is {}",
                status.state
            );
        }
        // Startup can fail after the exact surface handle is durably bound but before the
        // provider wrapper writes native-session.json. Explicit close may recover only that
        // bound launch surface; the adapter still targets its stable native identifiers.
        session.verify_managed_session(expected_session_id)?;
        // A Terminal.app window and tty are identities only inside the app incarnation
        // that no wrapper recorded here, so nothing is closed: the handle is consumed
        // once the window is proven gone.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if !terminal::facts(session.kind).failed_start_identity {
            if observation
                .evidence()
                .surface_absent(None, Box::new(surface_present))?
            {
                return Ok(TerminalCloseAuthority::Absent);
            }
            bail!(observation.evidence().startup_retained_reason());
        }
        if !terminal::facts(session.kind).failed_start_identity {
            bail!("startup surface has no supported native close identity; its handle is retained");
        }
        // No wrapper recorded an owner, so none can be live to close with.
        Ok(owner_exit::startup_surface(session, true))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    refuse_without_transport(ownership_proof)
}

// Other Unix targets have no visible terminal transport. The ownership proof still runs
// first, so its own refusal wins. Compiled into every test build so that CI runs it.
#[cfg(any(not(any(target_os = "macos", target_os = "linux", windows)), test))]
fn refuse_without_transport<T>(
    ownership_proof: impl FnOnce() -> Result<T>,
) -> Result<TerminalCloseAuthority> {
    ownership_proof()?;
    bail!(
        "this platform has no visible terminal transport (#6); no close was sent and the surface handle is retained"
    )
}

pub(in crate::native) fn close_owned_surface(
    directory: &Path,
    id: &str,
    session: &TerminalSession,
    cause: CloseCause,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    let observation = Production::new(directory, id, session, deadline).for_cause(cause);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return observation.close_posix_surface();
    #[cfg(windows)]
    return close_windows_owned_surface_with(
        || decide(&observation, id, session, deadline),
        terminal::windows_installed(),
        directory,
        session,
        deadline,
    );
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let authority = decide(&observation, id, session, deadline)?;
        if authority == TerminalCloseAuthority::Absent {
            return Ok(terminal::CloseOutcome::Missing.into());
        }
        terminal::close_session_until(directory, session, deadline).map(Into::into)
    }
}

/// Windows: the authority's action through the one Windows lookup. Compiled into the test
/// builds of macOS and Linux as well, so that the mapping runs over scripted adapters on every
/// CI target; the targets without a terminal transport have no `CreatedSurface` to map.
#[cfg(any(windows, all(test, any(target_os = "macos", target_os = "linux"))))]
pub(in crate::native) fn close_windows_owned_surface_with(
    authority: impl FnOnce() -> Result<TerminalCloseAuthority>,
    adapters: &dyn terminal::windows_dispatch::Adapters,
    directory: &Path,
    session: &TerminalSession,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    use terminal::windows_dispatch;
    close_remaining(deadline)?;
    let authority = authority()?;
    close_remaining(deadline)?;
    match authority {
        TerminalCloseAuthority::Absent => Ok(terminal::CloseOutcome::Missing.into()),
        TerminalCloseAuthority::CreatedSurface(running) => {
            windows_dispatch::close_created_surface(adapters, directory, session, running, deadline)
                .map(Into::into)
        }
        _ => windows_dispatch::close_session_until(adapters, directory, session, deadline)
            .map(Into::into),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Production<'_> {
    pub(in crate::native::terminal) fn close_posix_surface(&self) -> Result<terminal::CloseResult> {
        let Self {
            directory,
            expected_session_id: id,
            session,
            deadline,
            ..
        } = *self;
        close_owned_surface_with(
            || decide(self, id, session, deadline),
            || {
                #[cfg(test)]
                self.require_installed_observation("tear down the owner");
                teardown_absent_owner_with(
                    &Store::open_unchecked(directory),
                    id,
                    session,
                    || verified_posix_terminal_owner(directory, id, session, None),
                    |group| {
                        terminate_owned_foreground_group_until(group, deadline).context(
                            "could not confirm disappearance of the absent surface owner's group",
                        )
                    },
                    deadline,
                )
            },
            |close| {
                terminal::posix::close_owned_session(
                    self.posix_adapters,
                    directory,
                    id,
                    session,
                    close,
                    deadline,
                )
            },
            deadline,
        )
    }
}

// Production and interruption tests share the authority-to-action dispatch.
// Only observations and external actions are replaceable; result mapping is not.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn close_owned_surface_with(
    authority: impl FnOnce() -> Result<TerminalCloseAuthority>,
    teardown: impl FnOnce() -> Result<terminal::CloseResult>,
    close_surface: impl FnOnce(OwnedSurfaceClose) -> Result<CloseOutcome>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    close_remaining(deadline)?;
    let authority = authority()?;
    close_remaining(deadline)?;
    match authority {
        TerminalCloseAuthority::OwnerTeardown => teardown(),
        TerminalCloseAuthority::TeardownComplete(reason) => Ok(terminal::CloseResult {
            outcome: CloseOutcome::Missing,
            reason: Some(reason),
        }),
        TerminalCloseAuthority::Absent => Ok(CloseOutcome::Missing.into()),
        TerminalCloseAuthority::LiveOwner => {
            close_surface(OwnedSurfaceClose::LiveOwner).map(Into::into)
        }
        TerminalCloseAuthority::SurfaceOnly => {
            close_surface(OwnedSurfaceClose::SurfaceOnly).map(Into::into)
        }
        TerminalCloseAuthority::CreatedSurface(running) => {
            close_surface(OwnedSurfaceClose::CreatedSurface(running)).map(Into::into)
        }
    }
}

// Apple Terminal close-evidence fixtures; the shared rows run on Linux through the tmux kind.
#[cfg(all(test, target_os = "macos"))]
mod tests;

#[cfg(all(test, any(windows, target_os = "macos", target_os = "linux")))]
mod projected_authority_tests {
    use super::test_support::{RecordValues, TestObservation};
    use super::*;
    use crate::native::process_identity::{Attested, Gone};
    use crate::native::session::CoreRecord;
    use crate::native::session::launch;
    use crate::native::session::owner::ControlAttestation;

    #[test]
    fn other_unix_refusal_runs_the_proof_first_and_sends_no_close() {
        let error =
            refuse_without_transport(|| -> Result<()> { bail!("proof refused") }).unwrap_err();
        assert_eq!(error.to_string(), "proof refused");
        let error = refuse_without_transport(|| Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "this platform has no visible terminal transport (#6); no close was sent and the surface handle is retained"
        );
    }

    fn assert_failed_start_route(attestation: impl Fn() -> OwnerAttestation, surface_only: bool) {
        for kind in [
            terminal::TerminalKind::Iterm2,
            terminal::TerminalKind::Ghostty,
            terminal::TerminalKind::WindowsConsole,
        ] {
            for proof_succeeds in [false, true] {
                let surface = TerminalSession::new(kind, "4242".into())
                    .with_binding(Some("session-test".into()));
                let observation = TestObservation::from_values(RecordValues {
                    owner: Some(NativeSessionOwner {
                        pid: 4242,
                        managed_session_id: Some("session-test".into()),
                        ..Default::default()
                    }),
                    state: SessionState::Failed,
                    launch: Some(launch::Record {
                        schema: 1,
                        claim_token: "claim-test".into(),
                        deadline_unix_ms: 1,
                        phase: launch::Phase::Spawning,
                    }),
                })
                .with_owner_record(|_| attestation())
                .with_presence(|| Ok(true))
                .with_proof(|| {
                    if proof_succeeds {
                        Ok(())
                    } else {
                        bail!("injected full ownership proof failure")
                    }
                });
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                let observation = observation
                    .with_evidence(terminal::close_evidence(kind))
                    .with_posix_owner_live(|_| OwnerAttestation {
                        process: ProcessObservation::Live(Attested::Incarnation),
                        control: ControlAttestation::Verified,
                        pid_alive: Some(true),
                    });
                let result = observation.decide(
                    "session-test",
                    &surface,
                    crate::native::session::close::budget::close_deadline(),
                );
                let mut calls = vec![];
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                calls.push("teardown-intent");
                calls.extend(["owner-present", "launch", "status", "owner", "owner-record"]);
                if !surface_only {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    calls.extend(["resumable", "owner", "mac-live"]);
                    calls.push("proof");
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if !proof_succeeds {
                        calls.push("presence");
                    }
                }
                assert_eq!(
                    observation.calls(),
                    calls,
                    "{kind:?}, {:?}, proof succeeds {proof_succeeds}: {result:?}",
                    attestation()
                );
                if surface_only {
                    assert_eq!(result.unwrap(), TerminalCloseAuthority::SurfaceOnly);
                } else if proof_succeeds {
                    assert_eq!(result.unwrap(), TerminalCloseAuthority::LiveOwner);
                } else {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "injected full ownership proof failure"
                    );
                }
            }
        }
    }

    #[test]
    fn failed_start_typed_evidence_selects_shortcut_or_fresh_proof() {
        for control in [
            (|| ControlAttestation::Verified) as fn() -> ControlAttestation,
            || ControlAttestation::NotRecorded,
            || ControlAttestation::Changed("owner TTY changed".into()),
            || ControlAttestation::Unknown("control inspection failed".into()),
        ] {
            assert_failed_start_route(
                || OwnerAttestation {
                    process: ProcessObservation::Live(Attested::Incarnation),
                    control: control(),
                    pid_alive: Some(true),
                },
                false,
            );
        }
        assert_failed_start_route(
            || OwnerAttestation {
                process: ProcessObservation::Live(Attested::PidOnly),
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(true),
            },
            false,
        );
        for pid_alive in [Some(false), Some(true), None] {
            assert_failed_start_route(
                || OwnerAttestation {
                    process: ProcessObservation::Unknown("owner identity inspection failed".into()),
                    control: ControlAttestation::NotRecorded,
                    pid_alive,
                },
                false,
            );
        }
        assert_failed_start_route(
            || OwnerAttestation {
                process: ProcessObservation::Gone(Gone::Exited),
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(false),
            },
            true,
        );
        assert_failed_start_route(
            crate::native::session::owner::tests::reused_attestation,
            true,
        );
    }

    // os-parity: once the owner is observed gone, a Herdr handle is closed on its creation
    // record on every OS, whatever the owner record says, without the ownership proof or a
    // presence query. Each OS reaches that answer through its own owner observation. Only a
    // record bound to this session that names an incarnation lets the close end a running
    // terminal. An owner that cannot be inspected is not gone and keeps the live-owner rules,
    // and a close that cleans up after another command never acts on a gone owner's terminal.
    #[test]
    fn a_gone_owner_hands_a_herdr_close_to_its_creation_record_on_every_os() {
        fn incarnation(mut owner: NativeSessionOwner) -> NativeSessionOwner {
            #[cfg(target_os = "macos")]
            {
                owner.process_start_seconds = Some(1_790_000_000);
                owner.process_start_microseconds = Some(42);
            }
            #[cfg(target_os = "linux")]
            {
                owner.linux_birth = Some(crate::native::terminal::LinuxBirth {
                    boot_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
                    starttime_ticks: 42,
                });
            }
            #[cfg(windows)]
            {
                owner.windows_process_identity =
                    Some(crate::native::terminal::WindowsProcessIdentity {
                        creation_time: 42,
                        executable_path: r"C:\tabcli\tabcli.exe".into(),
                    });
            }
            owner
        }
        fn gone(process: Gone) -> OwnerAttestation {
            OwnerAttestation {
                process: ProcessObservation::Gone(process),
                control: ControlAttestation::NotRecorded,
                pid_alive: Some(false),
            }
        }
        let gone_ways: [fn() -> OwnerAttestation; 2] = [
            || gone(Gone::Exited),
            || gone(Gone::Reused("different birth".into())),
        ];
        let surface = TerminalSession::new(terminal::TerminalKind::Herdr, "t7".into())
            .with_binding(Some("session-test".into()));
        for (binding, recorded, running) in [
            (Some("session-test"), true, Running::End),
            (Some("session-test"), false, Running::Keep),
            (None, true, Running::Keep),
            (Some("session-foreign"), true, Running::Keep),
        ] {
            for observe in gone_ways {
                let owner = NativeSessionOwner {
                    pid: 4242,
                    managed_session_id: binding.map(str::to_owned),
                    ..Default::default()
                };
                let observation = TestObservation::from_values(RecordValues {
                    owner: Some(if recorded { incarnation(owner) } else { owner }),
                    state: SessionState::Ready,
                    launch: None,
                })
                .with_owner_record(move |_| observe())
                .with_presence(|| panic!("a gone owner's close asks no presence"))
                .with_proof(|| panic!("a gone owner's close needs no ownership proof"));
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                let observation = observation
                    .with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr))
                    .with_posix_owner_live(move |_| observe())
                    .with_pid_alive(|_| panic!("the creation record decides, not the PID"));
                let authority = observation
                    .decide(
                        "session-test",
                        &surface,
                        crate::native::session::close::budget::close_deadline(),
                    )
                    .unwrap();
                assert_eq!(
                    authority,
                    TerminalCloseAuthority::CreatedSurface(running),
                    "{binding:?}, incarnation {recorded}"
                );
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                let calls = [
                    "teardown-intent",
                    "owner-present",
                    "launch",
                    "owner",
                    "mac-live",
                ];
                #[cfg(windows)]
                let calls = ["owner-present", "launch", "owner", "owner-record"];
                assert_eq!(observation.calls(), calls, "{binding:?}");
            }
        }

        // Another session's handle is refused before anything is closed.
        let foreign = surface.clone().with_binding(Some("session-other".into()));
        let observation = TestObservation::from_values(RecordValues {
            owner: Some(NativeSessionOwner {
                pid: 4242,
                managed_session_id: Some("session-test".into()),
                ..Default::default()
            }),
            state: SessionState::Ready,
            launch: None,
        })
        .with_owner_record(|_| gone(Gone::Exited))
        .with_presence(|| panic!("a foreign handle asks no presence"))
        .with_proof(|| bail!("the owner is not proven live"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation = observation
            .with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr))
            .with_posix_owner_live(|_| gone(Gone::Exited))
            .with_pid_alive(|_| false);
        assert!(
            observation
                .decide(
                    "session-test",
                    &foreign,
                    crate::native::session::close::budget::close_deadline(),
                )
                .is_err()
        );

        // An owner that cannot be inspected keeps the live-owner rules, which refuse it.
        let observation = TestObservation::from_values(RecordValues {
            owner: Some(NativeSessionOwner {
                pid: 4242,
                managed_session_id: Some("session-test".into()),
                ..Default::default()
            }),
            state: SessionState::Ready,
            launch: None,
        })
        .with_owner_record(|_| OwnerAttestation {
            process: ProcessObservation::Unknown("owner inspection failed".into()),
            control: ControlAttestation::NotRecorded,
            pid_alive: None,
        })
        .with_presence(|| panic!("an uninspectable owner's close asks no presence"))
        .with_proof(|| bail!("the owner is not proven live"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation = observation
            .with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr))
            .with_posix_owner_live(|_| OwnerAttestation {
                process: ProcessObservation::Unknown("owner inspection failed".into()),
                control: ControlAttestation::NotRecorded,
                pid_alive: None,
            });
        assert!(
            observation
                .decide(
                    "session-test",
                    &surface,
                    crate::native::session::close::budget::close_deadline(),
                )
                .is_err()
        );

        // A cleanup after another command (a refused reopen) gains no authority over a gone
        // owner's terminal, even when its record would let an explicit close end it.
        let observation = TestObservation::from_values(RecordValues {
            owner: Some(incarnation(NativeSessionOwner {
                pid: 4242,
                managed_session_id: Some("session-test".into()),
                ..Default::default()
            })),
            state: SessionState::Ready,
            launch: None,
        })
        .with_cause(CloseCause::Cleanup)
        .with_owner_record(|_| gone(Gone::Exited))
        .with_presence(|| panic!("a cleanup asks no presence"))
        .with_proof(|| panic!("a cleanup of a gone owner asks no ownership proof"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation = observation
            .with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr))
            .with_posix_owner_live(|_| gone(Gone::Exited))
            .with_pid_alive(|_| false);
        let error = observation
            .decide(
                "session-test",
                &surface,
                crate::native::session::close::budget::close_deadline(),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only close-session --explicit ends or removes its Herdr terminal"),
            "{error:#}"
        );
    }

    #[test]
    fn r1_c6_windows_surface_failed_start_consumes_mismatch_projection() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::native::session::Store::open_unchecked(root.path());
        store
            .write_owner(&NativeSessionOwner {
                pid: 4242,
                managed_session_id: Some("session-test".into()),
                ..Default::default()
            })
            .unwrap();
        crate::native::session::update_status(root.path(), SessionState::Failed, None, None)
            .unwrap();
        store
            .record(CoreRecord::Launch)
            .write_json(&launch::Record {
                schema: 1,
                claim_token: "claim-test".into(),
                deadline_unix_ms: 1,
                phase: launch::Phase::Spawning,
            })
            .unwrap();
        let surface: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal":"windows-console", "session_id":"4242", "managed_session_id":"session-test"
        }))
        .unwrap();
        let deadline = crate::native::session::close::budget::close_deadline();
        let observation =
            TestObservation::on_records(root.path(), "session-test", &surface, deadline)
                .with_presence(|| panic!("projection does not inspect surface"))
                .with_owner_record(|_| crate::native::session::owner::tests::reused_attestation())
                .with_proof(|| panic!("failed-start route"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation = observation
            .with_pid_alive(|_| panic!("failed-start route"))
            .with_posix_owner_live(|_| panic!("failed-start route"))
            .with_resumable(|| panic!("failed-start route"))
            .with_attest(|| panic!("failed-start route"))
            .with_group_gone(|_| panic!("failed-start route"));
        let authority = observation
            .decide("session-test", &surface, deadline)
            .unwrap();
        assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
        // The failed-start projection reads the launch record, the status and the owner
        // in that order, from the real Records, and observes the owner once.
        let mut reads = vec![];
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        reads.push("teardown-intent");
        reads.extend(["owner-present", "launch", "status", "owner", "owner-record"]);
        assert_eq!(observation.calls(), reads);
    }
}
