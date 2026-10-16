//! Close authority: one decision over one observation seam, dispatched to the actions.
//!
//! The decision consults nothing but `CloseObservation`: the session's Records (read
//! when the decision asks, in the order it asks) and the OS and surface observations.
//! `Production` is the adapter that reads the session directory and observes the system;
//! tests use the in-memory adapter in `test_support`. The actions stay in `ownership`
//! (Terminal.app owner teardown) and the platform adapters (surface close).
use super as terminal;
#[cfg(target_os = "macos")]
use super::CloseOutcome;
use super::TerminalSession;
#[cfg(target_os = "macos")]
use super::macos::OwnedSurfaceClose;
#[cfg(target_os = "macos")]
use super::macos::ownership::verified_macos_terminal_owner;
#[cfg(any(target_os = "macos", windows))]
use super::ownership::NativeSessionOwner;
use super::ownership::verify_terminal_surface_ownership_with_timeout;
#[cfg(target_os = "macos")]
use super::ownership::{
    owner_group_gone, teardown_absent_owner_with, terminate_owned_foreground_group_until,
};
#[cfg(any(target_os = "macos", windows))]
use crate::native::SessionStatus;
#[cfg(target_os = "macos")]
use crate::native::process_identity::{Gone, ProcessObservation};
use crate::native::session::close::budget::close_remaining;
#[cfg(target_os = "macos")]
use crate::native::session::owner::{OwnerAttestation, attest_owner};
#[cfg(target_os = "macos")]
use crate::native::session::{self, Store};
#[cfg(any(target_os = "macos", windows))]
use crate::native::session::{
    CoreRecord, OwnerObservation, Reader, SessionState, launch, observe_owner_record,
};
#[cfg(target_os = "macos")]
use agent_bridge::process_is_alive;
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{Result, bail};
use std::{path::Path, time::Instant};

#[cfg(test)]
pub(in crate::native) mod test_support;

#[derive(Debug, Eq, PartialEq)]
pub(in crate::native) enum TerminalCloseAuthority {
    #[cfg(any(target_os = "macos", windows))]
    LiveOwner,
    #[cfg(target_os = "macos")]
    OwnerTeardown,
    #[cfg(target_os = "macos")]
    TeardownComplete(String),
    #[cfg(any(target_os = "macos", windows))]
    SurfaceOnly,
    Absent,
}

// Everything the decision consults. The Record accessors read when they are called, so
// the decision's call order is the Record-read order; the observations are the distinct
// OS and surface proofs that the production adapter keeps apart.
pub(in crate::native) trait CloseObservation {
    #[cfg(target_os = "macos")]
    fn teardown_intent(&self) -> Result<Option<session::close::TerminalTeardownIntent>>;
    #[cfg(any(target_os = "macos", windows))]
    fn owner_present(&self) -> Result<bool>;
    #[cfg(any(target_os = "macos", windows))]
    fn launch(&self) -> Result<Option<launch::Record>>;
    #[cfg(any(target_os = "macos", windows))]
    fn status(&self) -> Result<SessionStatus>;
    #[cfg(any(target_os = "macos", windows))]
    fn owner(&self) -> Result<NativeSessionOwner>;
    #[cfg(target_os = "macos")]
    fn resumable(&self) -> Result<bool>;
    fn surface_present(&self) -> Result<bool>;
    fn ownership_proof(&self) -> Result<()>;
    #[cfg(any(target_os = "macos", windows))]
    fn observe_owner_record(&self, owner: &NativeSessionOwner) -> OwnerObservation;
    #[cfg(target_os = "macos")]
    fn pid_alive(&self, pid: u32) -> bool;
    #[cfg(target_os = "macos")]
    fn mac_owner_live(&self, owner: &NativeSessionOwner) -> OwnerAttestation;
    #[cfg(target_os = "macos")]
    fn attest_owner(&self) -> Result<()>;
    #[cfg(target_os = "macos")]
    fn group_gone(&self, group: u32) -> Result<bool>;
    #[cfg(target_os = "macos")]
    fn evidence(&self) -> &dyn terminal::close_evidence::CloseEvidence;
}

// The production adapter: the session directory, read at each call, and the system.
// It holds no Record and takes no lock; the closer constructs it after the handle claim.
pub(in crate::native) struct Production<'a> {
    directory: &'a Path,
    expected_session_id: &'a str,
    session: &'a TerminalSession,
    deadline: Instant,
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
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    fn reader(&self) -> Reader {
        Reader::open_unchecked(self.directory)
    }
}

impl CloseObservation for Production<'_> {
    #[cfg(target_os = "macos")]
    fn teardown_intent(&self) -> Result<Option<session::close::TerminalTeardownIntent>> {
        session::close::terminal_teardown_intent(&self.reader(), self.session)
    }

    #[cfg(any(target_os = "macos", windows))]
    fn owner_present(&self) -> Result<bool> {
        Ok(self.reader().record(CoreRecord::Owner).text()?.is_some())
    }

    #[cfg(any(target_os = "macos", windows))]
    fn launch(&self) -> Result<Option<launch::Record>> {
        launch::read(&self.reader())
    }

    #[cfg(any(target_os = "macos", windows))]
    fn status(&self) -> Result<SessionStatus> {
        self.reader().status()
    }

    #[cfg(any(target_os = "macos", windows))]
    fn owner(&self) -> Result<NativeSessionOwner> {
        self.reader().owner()
    }

    #[cfg(target_os = "macos")]
    fn resumable(&self) -> Result<bool> {
        session::close::terminal_close_resumable(
            &self.reader(),
            self.session,
            terminal::surface_outlives_owner(self.session),
        )
    }

    fn surface_present(&self) -> Result<bool> {
        terminal::surface_present(
            self.directory,
            self.session,
            close_remaining(self.deadline)?,
        )
    }

    fn ownership_proof(&self) -> Result<()> {
        verify_terminal_surface_ownership_with_timeout(
            self.directory,
            self.expected_session_id,
            self.session,
            Some(close_remaining(self.deadline)?),
        )
    }

    #[cfg(any(target_os = "macos", windows))]
    fn observe_owner_record(&self, owner: &NativeSessionOwner) -> OwnerObservation {
        observe_owner_record(owner)
    }

    #[cfg(target_os = "macos")]
    fn pid_alive(&self, pid: u32) -> bool {
        process_is_alive(pid)
    }

    #[cfg(target_os = "macos")]
    fn mac_owner_live(&self, owner: &NativeSessionOwner) -> OwnerAttestation {
        attest_owner(owner)
    }

    #[cfg(target_os = "macos")]
    fn attest_owner(&self) -> Result<()> {
        verified_macos_terminal_owner(self.directory, self.expected_session_id, self.session, None)
            .map(|_| ())
    }

    #[cfg(target_os = "macos")]
    fn group_gone(&self, group: u32) -> Result<bool> {
        owner_group_gone(group)
    }

    #[cfg(target_os = "macos")]
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
    #[cfg(any(target_os = "macos", windows))]
    {
        #[cfg(target_os = "macos")]
        let teardown_intent = observation.teardown_intent()?;
        #[cfg(target_os = "macos")]
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
                    && owner.managed_session_id.as_deref() == Some(expected_session_id)
                    && (observed.process_alive == Some(false)
                        || observed.identity_matches == Some(false))
                {
                    session.verify_managed_session(expected_session_id)?;
                    return Ok(TerminalCloseAuthority::SurfaceOnly);
                }
            }
            // The close that recorded the intent may finish without signalling anything
            // again, and only once the incarnation it verified no longer exists. A live
            // or uninspectable incarnation keeps the rules below; a reused PID alone
            // grants nothing outside this exact-intent route.
            #[cfg(target_os = "macos")]
            if !has_teardown_intent && observation.resumable()? {
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
            #[cfg(target_os = "macos")]
            {
                let owner: NativeSessionOwner = observation.owner()?;
                let attestation = observation.mac_owner_live(&owner);
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
                    session.verify_managed_session(expected_session_id)?;
                    // A reused PID grants neither mutation nor absence authority. The
                    // original owner's complete binding is required even for a query.
                    if matches!(
                        attestation.process,
                        ProcessObservation::Gone(Gone::Reused(_))
                    ) || observation.pid_alive(owner.pid)
                        || owner.managed_session_id.as_deref() != Some(expected_session_id)
                        || !matches!(
                            (
                                owner.terminal_tty_device,
                                owner.process_start_seconds,
                                owner.process_start_microseconds,
                                owner.process_group,
                                owner.terminal_process_group
                            ),
                            (Some(_), Some(_), Some(_), Some(_), Some(_))
                        )
                        || owner.terminal_tty.as_deref().is_none_or(str::is_empty)
                    {
                        bail!(
                            "dead native-session owner identity is incomplete, foreign, or its PID is reused; no terminal observation or close was sent"
                        );
                    }
                    if let Some(intent) = teardown_intent {
                        if !observation.group_gone(intent.process_group)? {
                            bail!(
                                "the teardown owner's process group is not proven gone; no signal was sent and the handle is retained"
                            );
                        }
                        return Ok(TerminalCloseAuthority::TeardownComplete(intent.reason));
                    }
                    let absent = observation.evidence().surface_absent(Some(&owner), Box::new(surface_present))
                    .context("could not prove absence of the dead owner's exact terminal surface; no close was sent")?;
                    if absent {
                        return Ok(TerminalCloseAuthority::Absent);
                    }
                    bail!(
                        "the recorded native-session owner is no longer live; visible terminal cleanup is unverified and no close was sent"
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
        #[cfg(target_os = "macos")]
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
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        ownership_proof()?;
        bail!(
            "this platform has no visible terminal transport (#6); no close was sent and the surface handle is retained"
        )
    }
}

pub(in crate::native) fn close_owned_surface(
    directory: &Path,
    id: &str,
    session: &TerminalSession,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    let observation = Production::new(directory, id, session, deadline);
    #[cfg(target_os = "macos")]
    {
        close_owned_surface_with(
            || decide(&observation, id, session, deadline),
            || {
                teardown_absent_owner_with(
                    &Store::open_unchecked(directory),
                    id,
                    session,
                    || verified_macos_terminal_owner(directory, id, session, None),
                    |group| {
                        terminate_owned_foreground_group_until(group, deadline).context(
                            "could not confirm disappearance of the absent surface owner's group",
                        )
                    },
                    deadline,
                )
            },
            |has_native_owner| {
                terminal::macos::close_owned_session(
                    terminal::macos::INSTALLED,
                    directory,
                    id,
                    session,
                    if has_native_owner {
                        OwnedSurfaceClose::LiveOwner
                    } else {
                        OwnedSurfaceClose::SurfaceOnly
                    },
                    deadline,
                )
            },
            deadline,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let authority = decide(&observation, id, session, deadline)?;
        if authority == TerminalCloseAuthority::Absent {
            return Ok(terminal::CloseOutcome::Missing.into());
        }
        terminal::close_session_until(directory, session, deadline).map(Into::into)
    }
}

// Production and interruption tests share the authority-to-action dispatch.
// Only observations and external actions are replaceable; result mapping is not.
#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn close_owned_surface_with(
    authority: impl FnOnce() -> Result<TerminalCloseAuthority>,
    teardown: impl FnOnce() -> Result<terminal::CloseResult>,
    close_surface: impl FnOnce(bool) -> Result<CloseOutcome>,
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
        authority => close_surface(authority == TerminalCloseAuthority::LiveOwner).map(Into::into),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests;

#[cfg(all(test, any(windows, target_os = "macos")))]
mod projected_authority_tests {
    use super::test_support::TestObservation;
    use super::*;
    use crate::native::session::launch;

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
                .with_owner_record(|_| crate::native::session::owner::tests::reused_observation())
                .with_proof(|| panic!("failed-start route"));
        #[cfg(target_os = "macos")]
        let observation = observation
            .with_pid_alive(|_| panic!("failed-start route"))
            .with_mac_owner_live(|_| panic!("failed-start route"))
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
        #[cfg(target_os = "macos")]
        reads.push("teardown-intent");
        reads.extend(["owner-present", "launch", "status", "owner", "owner-record"]);
        assert_eq!(observation.calls(), reads);
    }
}
