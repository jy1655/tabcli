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
    /// The kind's core keeps its terminals and no live owner of this session holds the
    /// surface to close with. This is evidence, not a decision: the adapter's one granting
    /// function decides what may be sent from it and from its own observation of the terminal.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    CreatedSurface(CreationEvidence),
    Absent,
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(unused_imports)
)]
pub(in crate::native) use owner_exit::CreationEvidence;

/// What `CloseObservation::read_owner` found at the Owner record: one typed read, under the
/// record's read policy. A record that cannot be read is the read's error.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[derive(Debug)]
pub(in crate::native) enum OwnerRead {
    Absent,
    /// The bytes are not an Owner record.
    Malformed,
    Recorded(Box<NativeSessionOwner>),
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
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
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
    /// The Owner record in one typed read: absent, malformed or recorded; unreadable is the
    /// error.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn read_owner(&self) -> Result<OwnerRead>;
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
    #[cfg(windows)]
    fn console_owner(&self, owner: &NativeSessionOwner) -> ProcessObservation {
        self.observe_owner_record(owner).process
    }
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
        // Only actual absence can qualify a startup for ownerless surface close.
        Ok(!matches!(self.read_owner()?, OwnerRead::Absent))
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn read_owner(&self) -> Result<OwnerRead> {
        Ok(
            match crate::native::session::owner::observe(&self.reader()) {
                OwnerRecord::Absent => OwnerRead::Absent,
                OwnerRecord::Unreadable(error) => return Err(error),
                OwnerRecord::Malformed { .. } => OwnerRead::Malformed,
                OwnerRecord::Recorded(recorded) => OwnerRead::Recorded(Box::new(recorded.owner)),
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

    #[cfg(windows)]
    fn console_owner(&self, owner: &NativeSessionOwner) -> ProcessObservation {
        use crate::native::process_identity::{Attested, Gone};
        // A verified live wrapper only needs limited query access, as in the owned close.
        // The stronger synchronized observation distinguishes exit from inspection failure
        // when that proof is unavailable, including for PID-only records.
        if let Some(identity) = &owner.windows_process_identity
            && identity.creation_time != 0
            && !identity.executable_path.is_empty()
            && terminal::windows::verify_process_identity(
                owner.pid,
                identity.creation_time,
                &identity.executable_path,
            )
            .is_ok()
        {
            return ProcessObservation::Live(Attested::Incarnation);
        }
        match terminal::observe_windows_process_identity(owner.pid) {
            Err(error) => ProcessObservation::Unknown(format!("{error:#}")),
            Ok(None) => ProcessObservation::Gone(Gone::Exited),
            Ok(Some(live)) => match &owner.windows_process_identity {
                None => ProcessObservation::Live(Attested::PidOnly),
                Some(recorded)
                    if recorded.creation_time == 0 || recorded.executable_path.is_empty() =>
                {
                    ProcessObservation::Unknown(
                        "The recorded Windows owner has an incomplete process identity".into(),
                    )
                }
                Some(recorded) if recorded.creation_time != live.creation_time => {
                    ProcessObservation::Gone(Gone::Reused(format!(
                        "owner PID {} was reused",
                        owner.pid
                    )))
                }
                Some(recorded)
                    if recorded
                        .executable_path
                        .eq_ignore_ascii_case(&live.executable_path) =>
                {
                    ProcessObservation::Live(Attested::Incarnation)
                }
                Some(_) => ProcessObservation::Unknown(
                    "The recorded Windows owner executable identity changed".into(),
                ),
            },
        }
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

/// An ordinary unowned surface grants only settlement on positive absence. The adapter
/// may use a trustworthy recorded app identity, even if the wrapper itself has exited.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn unowned_close(
    observation: &dyn CloseObservation,
    id: &str,
    session: &TerminalSession,
    owner: Option<&NativeSessionOwner>,
    surface_present: impl FnOnce() -> Result<bool>,
) -> Result<TerminalCloseAuthority> {
    session.verify_managed_session(id)?;
    #[cfg(unix)]
    let absent = observation
        .evidence()
        .surface_absent(owner, Box::new(surface_present))?;
    #[cfg(windows)]
    let absent = {
        let _ = (observation, owner);
        !surface_present()?
    };
    if absent {
        return Ok(TerminalCloseAuthority::Absent);
    }
    bail!(
        "The recorded {} surface is still present. End it, then run `tabcli close-session {id} --explicit` again.",
        session.kind.display_name()
    )
}

// The decision. Its Record reads and their order are part of its contract: teardown
// intent first, owner presence by text, the launch record before the failed-start tests,
// then status and owner in that branch, resumability as a fresh call, the owner again
// before attestation, and status only in the no-owner branch. A kind whose core keeps its
// terminals reads the owner record once and, for a record bound to this session, its
// attestation, in `owner_exit::core_kept`.
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
        let core_kept_owner = if owner_exit::core_retains_finished(session.kind) {
            match owner_exit::core_kept(observation, expected_session_id, session)? {
                owner_exit::CoreKept::Created(evidence) => {
                    return Ok(TerminalCloseAuthority::CreatedSurface(evidence));
                }
                owner_exit::CoreKept::LiveOwner(observed) => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if let Some(intent) = &teardown_intent {
                        intent.verify_owner(&observed.0)?;
                    }
                    Some(*observed)
                }
            }
        } else {
            None
        };
        let ordinary = observation.cause() == CloseCause::Explicit
            && core_kept_owner.is_none()
            && !has_teardown_intent;
        let unowned = |owner: Option<&NativeSessionOwner>| {
            unowned_close(
                observation,
                expected_session_id,
                session,
                owner,
                surface_present,
            )
        };
        if core_kept_owner.is_some()
            || match observation.owner_present() {
                Err(_) if ordinary => return unowned(None),
                present => present?,
            }
        {
            // An explicit close may reclaim the exact startup surface after its wrapper
            // died, including an uncertain spawn. This is not automatic failure cleanup:
            // neither a live/unknown owner nor an unbound/foreign handle gains authority.
            // Terminal.app, Warp and WezTerm gain none here: a failed launch is no close
            // intent, and the dead-owner rules below apply to them.
            if core_kept_owner.is_none()
                && let Some(launch) = observation.launch()?
                && !has_teardown_intent
                && launch.phase != launch::Phase::Spawned
                && terminal::facts(session.kind).failed_start_identity
            {
                let status: SessionStatus = observation.status()?;
                let owner = match observation.owner() {
                    Err(_) if ordinary => return unowned(None),
                    owner => owner?,
                };
                if ordinary && owner.binding(expected_session_id) != OwnerBinding::Bound {
                    return unowned(None);
                }
                let observed = observation.observe_owner_record(&owner);
                if status.state == SessionState::Failed
                    && owner.binding(expected_session_id) == OwnerBinding::Bound
                    && matches!(observed.process, ProcessObservation::Gone(_))
                {
                    session.verify_managed_session(expected_session_id)?;
                    return Ok(TerminalCloseAuthority::SurfaceOnly);
                }
            }
            // The close that recorded the intent may finish without signalling anything
            // again, and only once the incarnation it verified no longer exists. A live
            // or uninspectable incarnation keeps the rules below; a reused PID alone
            // grants nothing outside this exact-intent route. A kind whose core keeps its
            // terminals needs no intent: a gone owner's close proves the terminal by its
            // creation record on every OS (`owner_exit`).
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if core_kept_owner.is_none() && !has_teardown_intent && observation.resumable()? {
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
                let (owner, attestation) = match core_kept_owner {
                    Some(observed) => observed,
                    None => {
                        let owner = match observation.owner() {
                            Err(_) if ordinary => return unowned(None),
                            owner => owner?,
                        };
                        if ordinary
                            && (owner.binding(expected_session_id) != OwnerBinding::Bound
                                || crate::native::session::owner::record_conflicts(&owner)
                                || !owner.control_record().is_complete()
                                || owner.terminal_tty.as_deref().is_none_or(str::is_empty))
                        {
                            return unowned(None);
                        }
                        let attestation = observation.posix_owner_live(&owner);
                        (owner, attestation)
                    }
                };
                if ordinary {
                    use crate::native::process_identity::Attested;
                    match &attestation.process {
                        ProcessObservation::Gone(_) => return unowned(Some(&owner)),
                        ProcessObservation::Unknown(_)
                        | ProcessObservation::Live(Attested::PidOnly) => return unowned(None),
                        ProcessObservation::Live(Attested::Incarnation) => {}
                    }
                }
                if let ProcessObservation::Unknown(reason) = &attestation.process {
                    bail!("{reason}");
                }
                match &attestation.control {
                    crate::native::session::owner::ControlAttestation::Changed(reason)
                    | crate::native::session::owner::ControlAttestation::Unknown(reason) => {
                        bail!("{reason}")
                    }
                    crate::native::session::owner::ControlAttestation::Exited if ordinary => {
                        return unowned(Some(&owner));
                    }
                    crate::native::session::owner::ControlAttestation::Exited => {
                        bail!("exited during attestation")
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
                if ordinary && session.kind == terminal::TerminalKind::WindowsConsole {
                    let owner = match observation.owner() {
                        Ok(owner) if owner.binding(expected_session_id) == OwnerBinding::Bound => {
                            owner
                        }
                        _ => return unowned(None),
                    };
                    let reason = match observation.console_owner(&owner) {
                        ProcessObservation::Gone(_) => return unowned(None),
                        ProcessObservation::Unknown(reason) => reason,
                        ProcessObservation::Live(
                            crate::native::process_identity::Attested::PidOnly,
                        ) => format!(
                            "The recorded owner PID {} is live, but its process identity was not recorded",
                            owner.pid
                        ),
                        ProcessObservation::Live(_)
                            if crate::native::session::owner::record_conflicts(&owner) =>
                        {
                            "The recorded owner has conflicting platform identities".to_owned()
                        }
                        ProcessObservation::Live(_) => match ownership_proof() {
                            Ok(()) => return Ok(TerminalCloseAuthority::LiveOwner),
                            Err(error) => {
                                unowned(None)?;
                                return Err(error);
                            }
                        },
                    };
                    unowned(None)?;
                    bail!("{reason}");
                }
                ownership_proof()?;
                return Ok(TerminalCloseAuthority::LiveOwner);
            }
        }
        let status: SessionStatus = observation.status()?;
        if !matches!(status.state, SessionState::Launching | SessionState::Failed) {
            if ordinary {
                return unowned(None);
            }
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
        Ok(TerminalCloseAuthority::SurfaceOnly)
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
    // Check the binding before refusing an unavailable surface.
    if let Some(os) = terminal::OsTerminals::CURRENT
        && !os.runs(session.kind)
    {
        close_remaining(deadline)?;
        session.verify_managed_session(id)?;
        return Err(os.unavailable(session.kind));
    }
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
        TerminalCloseAuthority::Absent => {
            if session.kind == terminal::TerminalKind::WindowsConsole {
                adapters
                    .adapter(session.kind)?
                    .end_recorded_tab_host(directory, deadline)?;
                close_remaining(deadline)?;
            }
            Ok(terminal::CloseOutcome::Missing.into())
        }
        TerminalCloseAuthority::CreatedSurface(evidence) => {
            let generated = evidence.generated_reason();
            windows_dispatch::close_created_surface(
                adapters, directory, session, evidence, deadline,
            )
            .map(|outcome| terminal::CloseResult::keeping_diagnosis(outcome, generated))
        }
        // Retained adapter mapping. No current decision selects SurfaceOnly for Herdr.
        TerminalCloseAuthority::SurfaceOnly if owner_exit::core_retains_finished(session.kind) => {
            windows_dispatch::close_created_surface(
                adapters,
                directory,
                session,
                CreationEvidence::surface_only(),
                deadline,
            )
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
            reason: Some(reason),
            ..CloseOutcome::Missing.into()
        }),
        TerminalCloseAuthority::Absent => Ok(CloseOutcome::Missing.into()),
        TerminalCloseAuthority::LiveOwner => {
            close_surface(OwnedSurfaceClose::LiveOwner).map(Into::into)
        }
        TerminalCloseAuthority::SurfaceOnly => {
            close_surface(OwnedSurfaceClose::SurfaceOnly).map(Into::into)
        }
        TerminalCloseAuthority::CreatedSurface(evidence) => {
            let generated = evidence.generated_reason();
            close_surface(OwnedSurfaceClose::CreatedSurface(evidence))
                .map(|outcome| terminal::CloseResult::keeping_diagnosis(outcome, generated))
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
                let owner = owner_record(Record::Bound).unwrap();
                #[cfg(unix)]
                let owner = NativeSessionOwner {
                    terminal_tty: Some("/dev/test".into()),
                    terminal_tty_device: Some(1),
                    process_group: Some(4242),
                    terminal_process_group: Some(4242),
                    ..owner
                };
                let first = std::cell::Cell::new(true);
                let observation = TestObservation::from_values(RecordValues {
                    owner: Some(owner),
                    state: SessionState::Failed,
                    launch: Some(launch::Record {
                        schema: 1,
                        claim_token: "claim-test".into(),
                        deadline_unix_ms: 1,
                        phase: launch::Phase::Spawning,
                    }),
                })
                .with_owner_record(|_| {
                    if first.replace(false) {
                        attestation()
                    } else {
                        OwnerAttestation {
                            process: ProcessObservation::Live(Attested::Incarnation),
                            control: ControlAttestation::Verified,
                            pid_alive: Some(true),
                        }
                    }
                })
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
                    #[cfg(windows)]
                    if kind == terminal::TerminalKind::WindowsConsole {
                        calls.extend(["owner", "owner-record"]);
                    }
                    calls.push("proof");
                    if !proof_succeeds
                        && (!cfg!(windows) || kind == terminal::TerminalKind::WindowsConsole)
                    {
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
                        if cfg!(windows) && kind == terminal::TerminalKind::WindowsConsole {
                            "The recorded Windows Console surface is still present. End it, then run `tabcli close-session session-test --explicit` again."
                        } else {
                            "injected full ownership proof failure"
                        }
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

    /// The owner record a row starts from.
    #[derive(Clone, Copy, Debug)]
    enum Record {
        /// No owner record at all.
        Absent,
        /// A record that cannot be decoded.
        Damaged,
        Unreadable,
        /// Bound to this session, naming an incarnation this platform can produce.
        Bound,
        /// Bound, with another platform's birth beside its own: attestation answers Unknown.
        Conflicting,
        /// Bound, with a third platform's incarnation fields beside its own, which this
        /// platform's attestation does not read: a matching live incarnation can be attested.
        ConflictingUnread,
        /// Bound, with another platform's identity nested in the record: on Windows and Linux a
        /// Terminal.app incarnation, on macOS a shell recorded with a Linux birth.
        ConflictingNested,
        /// Bound, PID only.
        PidOnly,
        Unbound,
        Foreign,
    }

    fn owner_record(record: Record) -> Option<NativeSessionOwner> {
        let mut owner = NativeSessionOwner {
            pid: 4242,
            managed_session_id: Some("session-test".into()),
            ..Default::default()
        };
        let incarnation = |owner: &mut NativeSessionOwner| {
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
        };
        match record {
            Record::Absent | Record::Damaged | Record::Unreadable => return None,
            Record::Bound | Record::Unbound | Record::Foreign => incarnation(&mut owner),
            Record::Conflicting => {
                incarnation(&mut owner);
                #[cfg(any(target_os = "macos", windows))]
                {
                    owner.linux_birth = Some(crate::native::terminal::LinuxBirth {
                        boot_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
                        starttime_ticks: 42,
                    });
                }
                #[cfg(target_os = "linux")]
                {
                    owner.process_start_seconds = Some(1_790_000_000);
                }
            }
            Record::ConflictingNested => {
                incarnation(&mut owner);
                #[cfg(any(target_os = "linux", windows))]
                {
                    owner.terminal_app =
                        Some(crate::native::terminal::ownership::MacTerminalAppIdentity {
                            pid: 501,
                            start_seconds: 1_790_000_000,
                            start_microseconds: 7,
                        });
                }
                #[cfg(target_os = "macos")]
                {
                    owner.terminal_shell =
                        Some(crate::native::terminal::ownership::TerminalShellIdentity {
                            pid: 4241,
                            process_group: 4241,
                            terminal_tty_device: 42,
                            process_start_seconds: 0,
                            process_start_microseconds: 0,
                            linux_birth: Some(crate::native::terminal::LinuxBirth {
                                boot_id: "00112233-4455-6677-8899-aabbccddeeff".parse().unwrap(),
                                starttime_ticks: 41,
                            }),
                        });
                }
            }
            Record::ConflictingUnread => {
                incarnation(&mut owner);
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    owner.windows_process_identity =
                        Some(crate::native::terminal::WindowsProcessIdentity {
                            creation_time: 42,
                            executable_path: r"C:\tabcli\tabcli.exe".into(),
                        });
                }
                #[cfg(windows)]
                {
                    owner.process_start_seconds = Some(1_790_000_000);
                    owner.process_start_microseconds = Some(42);
                }
            }
            Record::PidOnly => {}
        }
        match record {
            Record::Unbound => owner.managed_session_id = None,
            Record::Foreign => owner.managed_session_id = Some("session-foreign".into()),
            _ => {}
        }
        Some(owner)
    }

    fn attestation(process: fn() -> ProcessObservation) -> OwnerAttestation {
        OwnerAttestation {
            process: process(),
            control: ControlAttestation::NotRecorded,
            pid_alive: None,
        }
    }

    #[test]
    fn ordinary_close_without_verified_owner_requires_positive_absence() {
        use crate::native::session::Store;
        #[cfg(windows)]
        let kinds = [terminal::TerminalKind::WindowsConsole];
        #[cfg(unix)]
        let kinds = [
            terminal::TerminalKind::Ghostty,
            terminal::TerminalKind::Iterm2,
            terminal::TerminalKind::Warp,
            terminal::TerminalKind::WezTerm,
        ];
        for kind in kinds {
            for (record, process) in [
                (
                    Record::Absent,
                    (|| ProcessObservation::Gone(Gone::Exited)) as fn() -> _,
                ),
                (Record::Damaged, || ProcessObservation::Gone(Gone::Exited)),
                (Record::Unreadable, || {
                    ProcessObservation::Gone(Gone::Exited)
                }),
                (Record::Foreign, || {
                    ProcessObservation::Live(Attested::Incarnation)
                }),
                (Record::Foreign, || ProcessObservation::Gone(Gone::Exited)),
                (Record::Unbound, || {
                    ProcessObservation::Live(Attested::Incarnation)
                }),
                (Record::Bound, || {
                    ProcessObservation::Gone(Gone::Reused("different birth".into()))
                }),
                (Record::Bound, || {
                    ProcessObservation::Unknown("owner inspection denied".into())
                }),
                (Record::PidOnly, || {
                    ProcessObservation::Live(Attested::PidOnly)
                }),
                (Record::ConflictingNested, || {
                    ProcessObservation::Live(Attested::Incarnation)
                }),
            ] {
                for presence in [Some(false), Some(true), None] {
                    let dir = tempfile::tempdir().unwrap();
                    let store = Store::open_unchecked(dir.path());
                    if let Some(owner) = owner_record(record) {
                        store.write_owner(&owner).unwrap();
                    } else if matches!(record, Record::Damaged) {
                        std::fs::write(store.record(CoreRecord::Owner).path(), b"{").unwrap();
                    } else if matches!(record, Record::Unreadable) {
                        std::fs::create_dir(store.record(CoreRecord::Owner).path()).unwrap();
                    }
                    let surface = TerminalSession::new(kind, "4242".into())
                        .with_binding(Some("session-test".into()));
                    let deadline = crate::native::session::close::budget::close_deadline();
                    let observation =
                        TestObservation::on_records(dir.path(), "session-test", &surface, deadline)
                            .with_launch(|| Ok(None))
                            .with_status(|| {
                                Ok(SessionStatus {
                                    state: SessionState::Ready,
                                    generation: 0,
                                    updated_unix_ms: 1,
                                    exit_code: None,
                                    error: None,
                                    residual_surface: None,
                                })
                            })
                            .with_owner_record(|_| attestation(process))
                            .with_presence(|| {
                                presence
                                    .ok_or_else(|| anyhow::anyhow!("terminal inspection denied"))
                            })
                            .with_proof(|| {
                                panic!("ordinary ownerless close cannot call owned close")
                            });
                    #[cfg(unix)]
                    let observation = observation
                        .with_evidence(terminal::close_evidence(kind))
                        .with_resumable(|| Ok(false))
                        .with_pid_alive(|_| true)
                        .with_posix_owner_live(|_| attestation(process));
                    let result = observation.decide("session-test", &surface, deadline);
                    #[cfg(windows)]
                    let result = match result {
                        Err(error) => {
                            // An R7 refusal must reach neither the helper nor the tab host.
                            struct NoCalls;
                            impl terminal::windows_dispatch::Adapters for NoCalls {
                                fn adapter(
                                    &self,
                                    _: terminal::TerminalKind,
                                ) -> Result<&dyn terminal::windows_dispatch::WindowsTerminalAdapter>
                                {
                                    panic!("refused console close reached an adapter");
                                }
                            }
                            Err(close_windows_owned_surface_with(
                                || Err(error),
                                &NoCalls,
                                dir.path(),
                                &surface,
                                deadline,
                            )
                            .unwrap_err())
                        }
                        ok => ok,
                    };
                    let label = format!("{kind:?} {record:?} {:?} {presence:?}", process());
                    match presence {
                        None => assert_eq!(
                            result.unwrap_err().root_cause().to_string(),
                            "terminal inspection denied",
                            "{label}"
                        ),
                        Some(true) => assert_eq!(
                            result.unwrap_err().to_string(),
                            format!(
                                "The recorded {} surface is still present. End it, then run `tabcli close-session session-test --explicit` again.",
                                kind.display_name()
                            ),
                            "{label}"
                        ),
                        #[cfg(windows)]
                        Some(false) if matches!(process(), ProcessObservation::Unknown(_)) => {
                            assert_eq!(
                                result.unwrap_err().to_string(),
                                "owner inspection denied",
                                "{label}"
                            )
                        }
                        #[cfg(windows)]
                        Some(false)
                            if matches!(record, Record::PidOnly | Record::ConflictingNested) =>
                        {
                            assert_eq!(
                                result.unwrap_err().to_string(),
                                if matches!(record, Record::PidOnly) {
                                    "The recorded owner PID 4242 is live, but its process identity was not recorded"
                                } else {
                                    "The recorded owner has conflicting platform identities"
                                },
                                "{label}"
                            )
                        }
                        Some(false) => {
                            assert_eq!(result.unwrap(), TerminalCloseAuthority::Absent, "{label}")
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn failed_spawning_malformed_owner_observes_absence_without_startup_grant() {
        use crate::native::session::Store;
        #[cfg(windows)]
        let kinds = [terminal::TerminalKind::WindowsConsole];
        #[cfg(unix)]
        let kinds = [
            terminal::TerminalKind::Ghostty,
            terminal::TerminalKind::Iterm2,
        ];
        for kind in kinds {
            let surface =
                TerminalSession::new(kind, "42".into()).with_binding(Some("session-test".into()));
            for malformed in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open_unchecked(directory.path());
                if malformed {
                    std::fs::write(store.record(CoreRecord::Owner).path(), b"{").unwrap();
                }
                store
                    .write_status(&SessionStatus {
                        state: SessionState::Failed,
                        generation: 0,
                        updated_unix_ms: 1,
                        exit_code: None,
                        error: None,
                        residual_surface: None,
                    })
                    .unwrap();
                store
                    .record(CoreRecord::Launch)
                    .write_json(&launch::Record {
                        schema: 1,
                        claim_token: "claim".into(),
                        deadline_unix_ms: 1,
                        phase: launch::Phase::Spawning,
                    })
                    .unwrap();
                for present in [false, true] {
                    let deadline = crate::native::session::close::budget::close_deadline();
                    let observation = TestObservation::on_records(
                        directory.path(),
                        "session-test",
                        &surface,
                        deadline,
                    )
                    .with_presence(|| {
                        assert!(malformed);
                        Ok(present)
                    });
                    #[cfg(unix)]
                    let observation = observation.with_evidence(terminal::close_evidence(kind));
                    let result = observation.decide("session-test", &surface, deadline);
                    if malformed && present {
                        assert_eq!(
                            result.unwrap_err().to_string(),
                            format!(
                                "The recorded {} surface is still present. End it, then run `tabcli close-session session-test --explicit` again.",
                                kind.display_name()
                            )
                        );
                    } else {
                        assert_eq!(
                            result.unwrap(),
                            if malformed {
                                TerminalCloseAuthority::Absent
                            } else {
                                TerminalCloseAuthority::SurfaceOnly
                            }
                        );
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_disappearing_during_control_attestation_requires_independent_absence() {
        let kind = terminal::TerminalKind::Ghostty;
        let surface =
            TerminalSession::new(kind, "42".into()).with_binding(Some("session-test".into()));
        let owner = NativeSessionOwner {
            terminal_tty: Some("/dev/test".into()),
            terminal_tty_device: Some(1),
            process_group: Some(4242),
            terminal_process_group: Some(4242),
            ..owner_record(Record::Bound).unwrap()
        };
        let observation = TestObservation::from_values(RecordValues {
            owner: Some(owner),
            state: SessionState::Ready,
            launch: None,
        })
        .with_posix_owner_live(|_| OwnerAttestation {
            process: ProcessObservation::Live(Attested::Incarnation),
            control: ControlAttestation::Exited,
            pid_alive: Some(true),
        })
        .with_evidence(terminal::close_evidence(kind))
        .with_presence(|| Ok(false));
        assert_eq!(
            observation
                .decide(
                    "session-test",
                    &surface,
                    crate::native::session::close::budget::close_deadline()
                )
                .unwrap(),
            TerminalCloseAuthority::Absent
        );
    }

    #[test]
    fn cleanup_gains_no_absence_authority_for_a_ready_session_without_an_owner() {
        let kind = if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::Ghostty
        };
        let surface =
            TerminalSession::new(kind, "42".into()).with_binding(Some("session-test".into()));
        let observation = TestObservation::from_values(RecordValues {
            owner: None,
            state: SessionState::Ready,
            launch: None,
        })
        .with_cause(CloseCause::Cleanup)
        .with_presence(|| panic!("cleanup has no independent absence grant"));
        assert_eq!(
            observation
                .decide(
                    "session-test",
                    &surface,
                    crate::native::session::close::budget::close_deadline()
                )
                .unwrap_err()
                .to_string(),
            "terminal close requires a live native-session owner while the session is ready"
        );
    }

    /// What the decision answers for a row.
    #[derive(Debug, Eq, PartialEq)]
    enum Answer {
        /// A close on the creation record, explicit or not, and whether the closed record gets
        /// the generated diagnosis (this session's owner observed gone). Neither permits ending
        /// a running terminal.
        Created {
            explicit: bool,
            diagnosed: bool,
        },
        LiveOwner,
        Refused,
    }

    // os-parity: every branch of the decision for a kind whose core keeps its terminals, on
    // every OS, against the one function that chooses (`owner_exit::core_kept`). Each OS reaches
    // it through its own owner observation. Only a record bound to this session, whose fields do
    // not contradict each other, with an attested live incarnation reaches the live-owner rules.
    // Every other row is a close on the creation record, which never ends a running terminal
    // (the owner's decision of 2026-11-26); an owner observed gone only adds the closed record's
    // diagnosis. A cleanup's evidence is never explicit. The ownership proof and
    // the presence query are asked only on the live-owner row.
    #[test]
    fn every_herdr_branch_returns_evidence_from_one_function_on_every_os() {
        let exited = || ProcessObservation::Gone(Gone::Exited);
        let reused = || ProcessObservation::Gone(Gone::Reused("different birth".into()));
        let live = || ProcessObservation::Live(Attested::Incarnation);
        let live_pid = || ProcessObservation::Live(Attested::PidOnly);
        let unknown = || ProcessObservation::Unknown("owner inspection failed".into());
        let created = |diagnosed| Answer::Created {
            explicit: true,
            diagnosed,
        };
        type Row = (
            &'static str,
            Record,
            fn() -> ProcessObservation,
            SessionState,
            Option<launch::Phase>,
            CloseCause,
            Answer,
        );
        let rows: Vec<Row> = vec![
            (
                "no owner record",
                Record::Absent,
                exited,
                SessionState::Failed,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "no owner record, the session ended",
                Record::Absent,
                exited,
                SessionState::Exited,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "damaged owner record",
                Record::Damaged,
                exited,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "bound, live",
                Record::Bound,
                live,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                Answer::LiveOwner,
            ),
            (
                "bound, exited",
                Record::Bound,
                exited,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(true),
            ),
            (
                "bound, reused",
                Record::Bound,
                reused,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(true),
            ),
            (
                "conflicting record",
                Record::Conflicting,
                unknown,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "PID-only, exited",
                Record::PidOnly,
                exited,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(true),
            ),
            (
                "PID-only, a live process",
                Record::PidOnly,
                live_pid,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "unbound, exited",
                Record::Unbound,
                exited,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "another session's live owner",
                Record::Foreign,
                live,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "another session's owner, the session ended",
                Record::Foreign,
                live,
                SessionState::Exited,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "failed start, bound, exited",
                Record::Bound,
                exited,
                SessionState::Failed,
                Some(launch::Phase::Spawning),
                CloseCause::Explicit,
                created(true),
            ),
            (
                "failed start, PID-only",
                Record::PidOnly,
                exited,
                SessionState::Failed,
                Some(launch::Phase::Spawning),
                CloseCause::Explicit,
                created(true),
            ),
            // This session's owner that cannot be inspected decides
            // nothing; the creation record removes a finished terminal and settles an absent one.
            (
                "uninspectable owner",
                Record::Bound,
                unknown,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            // A record that carries another platform's incarnation
            // fields contradicts itself, even with a matching live local incarnation.
            (
                "contradictory record, another platform's birth, live",
                Record::Conflicting,
                live,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "contradictory record, a third platform's incarnation, live",
                Record::ConflictingUnread,
                live,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            // The whole decoded record, nested identities included.
            (
                "contradictory record, another platform's nested identity, live",
                Record::ConflictingNested,
                live,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            // Another session's or an unbound record's process is not inspected.
            (
                "another session's owner, uninspectable",
                Record::Foreign,
                unknown,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            (
                "unbound owner, uninspectable",
                Record::Unbound,
                unknown,
                SessionState::Ready,
                None,
                CloseCause::Explicit,
                created(false),
            ),
            // A verified live owner of this session still permits the cleanup after a refused
            // reopen, through the close with a live owner; only the creation record refuses it.
            (
                "cleanup, bound, live",
                Record::Bound,
                live,
                SessionState::Ready,
                None,
                CloseCause::Cleanup,
                Answer::LiveOwner,
            ),
            (
                "cleanup, bound, exited",
                Record::Bound,
                exited,
                SessionState::Ready,
                None,
                CloseCause::Cleanup,
                Answer::Created {
                    explicit: false,
                    diagnosed: true,
                },
            ),
            (
                "cleanup, failed start",
                Record::Bound,
                exited,
                SessionState::Failed,
                Some(launch::Phase::Spawning),
                CloseCause::Cleanup,
                Answer::Created {
                    explicit: false,
                    diagnosed: true,
                },
            ),
        ];
        let surface = TerminalSession::new(terminal::TerminalKind::Herdr, "t7".into())
            .with_binding(Some("session-test".into()));
        for (label, record, process, state, phase, cause, expected) in rows {
            let observation = TestObservation::from_values(RecordValues {
                owner: owner_record(record),
                state,
                launch: phase.map(|phase| launch::Record {
                    schema: 1,
                    claim_token: "claim-test".into(),
                    deadline_unix_ms: 1,
                    phase,
                }),
            })
            .with_cause(cause)
            .with_owner_record(move |_| attestation(process))
            .with_presence(|| panic!("no Herdr row asks presence"));
            let observation = if matches!(record, Record::Damaged) {
                observation.with_read_owner(|| Ok(OwnerRead::Malformed))
            } else {
                observation
            };
            let live_owner_row = matches!(expected, Answer::LiveOwner);
            let observation = observation.with_proof(move || {
                assert!(
                    live_owner_row,
                    "only a live owner's close asks the ownership proof"
                );
                Ok(())
            });
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let observation = observation
                .with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr))
                .with_posix_owner_live(move |_| OwnerAttestation {
                    control: if matches!(process(), ProcessObservation::Live(Attested::Incarnation))
                    {
                        ControlAttestation::Verified
                    } else {
                        ControlAttestation::NotRecorded
                    },
                    ..attestation(process)
                })
                .with_pid_alive(|_| panic!("the PID probe decides nothing for a Herdr terminal"));
            let answer = match observation.decide(
                "session-test",
                &surface,
                crate::native::session::close::budget::close_deadline(),
            ) {
                Ok(TerminalCloseAuthority::CreatedSurface(evidence)) => Answer::Created {
                    explicit: evidence.explicit(),
                    diagnosed: evidence.generated_reason().is_some(),
                },
                Ok(TerminalCloseAuthority::LiveOwner) => Answer::LiveOwner,
                Ok(other) => panic!("{label}: {other:?}"),
                Err(_) => Answer::Refused,
            };
            assert_eq!(answer, expected, "{label}");
        }

        // Another session's handle is refused before anything is read about its owner.
        let foreign = surface.clone().with_binding(Some("session-other".into()));
        let observation = TestObservation::from_values(RecordValues {
            owner: owner_record(Record::Bound),
            state: SessionState::Ready,
            launch: None,
        })
        .with_owner_record(|_| panic!("a foreign handle observes no owner"))
        .with_presence(|| panic!("a foreign handle asks no presence"))
        .with_proof(|| panic!("a foreign handle asks no proof"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation =
            observation.with_posix_owner_live(|_| panic!("a foreign handle observes no owner"));
        assert!(
            observation
                .decide(
                    "session-test",
                    &foreign,
                    crate::native::session::close::budget::close_deadline(),
                )
                .is_err()
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

    // A kind whose core keeps its terminals reads the owner record
    // once, as one typed result. The scripted presence stands for an earlier read that found the
    // record, and the directory now in its place for the record replaced before a later read:
    // with one read, that read meets the replacement. Its failure verifies no live owner, so the
    // close takes the creation record, with the failure kept for the
    // refusal of a running terminal.
    #[test]
    fn a_core_kept_terminal_reads_its_owner_record_once() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::native::session::Store::open_unchecked(root.path());
        crate::native::session::update_status(root.path(), SessionState::Ready, None, None)
            .unwrap();
        std::fs::create_dir(store.record(CoreRecord::Owner).path()).unwrap();
        let surface = TerminalSession::new(terminal::TerminalKind::Herdr, "t7".into())
            .with_binding(Some("session-test".into()));
        let deadline = crate::native::session::close::budget::close_deadline();
        let observation =
            TestObservation::on_records(root.path(), "session-test", &surface, deadline)
                .with_owner_present(|| Ok(true))
                .with_presence(|| panic!("an unreadable owner record asks no presence"))
                .with_proof(|| panic!("an unreadable owner record asks no proof"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observation =
            observation.with_evidence(terminal::close_evidence(terminal::TerminalKind::Herdr));
        let result = observation.decide("session-test", &surface, deadline);
        let Ok(TerminalCloseAuthority::CreatedSurface(evidence)) = &result else {
            panic!("{result:?}");
        };
        let unverified = evidence.unverified().unwrap_or_default();
        assert!(
            unverified.contains("its owner record could not be read"),
            "{unverified}"
        );
        let calls = observation.calls();
        assert_eq!(
            calls.iter().filter(|call| **call == "read-owner").count(),
            1,
            "{calls:?}"
        );
        assert!(
            !calls.contains(&"owner-present") && !calls.contains(&"owner"),
            "{calls:?}"
        );
    }
}
