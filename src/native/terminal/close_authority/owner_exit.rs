//! Surface decisions after the recorded owner has gone.
//!
//! One rule on every OS for a kind whose core keeps its terminals (Herdr; the owner's decision
//! of 2026-11-26, os-parity): Repair never ends or removes such a terminal and leaves its
//! session for `close-session --explicit`, and that close ends and removes the terminal on its
//! own creation record once the owner is verified gone, whatever the owner record says (exited
//! or PID reused; bound, unbound or another session's; complete or not). The owner PID alone
//! never proves the terminal: the adapter does, from the creation record in the handle.
//!
//! Other kinds keep their platform's rule: macOS and Linux Repair retain a handle that can
//! outlive the owner, and a close intent; Windows Repair closes the managed console of an
//! active session through its root identity and settles an ended one from its Records.
use super::super::{self as terminal, CloseOutcome, TerminalKind, TerminalSession};
use crate::native::process_identity::ProcessObservation;
use crate::native::session::close::budget::close_remaining;
use crate::native::session::owner::OwnerAttestation;
use crate::native::session::{Reader, SessionState};
use anyhow::{Result, bail};
use std::time::Instant;
use terminal::facts::OwnerSurvival;

pub(in crate::native) fn repair_owner_is_live(attestation: &OwnerAttestation) -> Result<bool> {
    match &attestation.process {
        ProcessObservation::Live(_) => Ok(true),
        ProcessObservation::Gone(_) => Ok(false),
        ProcessObservation::Unknown(reason) => bail!("{reason}"),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native::terminal) fn outlives_owner(kind: TerminalKind) -> bool {
    terminal::facts(kind).owner_survival != OwnerSurvival::NoRetention
}

/// A kind whose core keeps its terminals until they are removed. Its handle records the
/// terminal's creation (core, address, creation time, program and launch label), which proves
/// the terminal without the owner.
pub(in crate::native::terminal) fn core_retains_finished(kind: TerminalKind) -> bool {
    terminal::facts(kind).owner_survival == OwnerSurvival::CoreRetainsFinished
}

/// The authority over a surface whose recorded owner is verified gone, the same on every OS:
/// a kind whose core keeps its terminals is closed on the terminal's creation record. `None`
/// leaves the decision to the platform's rule for the other kinds.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn gone_owner(
    session: &TerminalSession,
    expected_session_id: &str,
) -> Result<Option<super::TerminalCloseAuthority>> {
    if !core_retains_finished(session.kind) {
        return Ok(None);
    }
    session.verify_managed_session(expected_session_id)?;
    Ok(Some(super::TerminalCloseAuthority::CreatedSurface))
}

/// Windows explicit close: a kind whose core keeps its terminals is observed for a gone owner
/// before the ownership proof, which a gone owner never passes. A live or uninspectable
/// owner keeps the ownership proof.
#[cfg(windows)]
pub(super) fn windows_explicit_close(
    observation: &dyn super::CloseObservation,
    expected_session_id: &str,
    session: &TerminalSession,
) -> Result<Option<super::TerminalCloseAuthority>> {
    if !core_retains_finished(session.kind) {
        return Ok(None);
    }
    let owner = observation.owner()?;
    if !matches!(
        observation.observe_owner_record(&owner).process,
        ProcessObservation::Gone(_)
    ) {
        return Ok(None);
    }
    gone_owner(session, expected_session_id)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn explicit_close(
    observation: &dyn super::CloseObservation,
    expected_session_id: &str,
    session: &TerminalSession,
    owner: &terminal::ownership::NativeSessionOwner,
    process: ProcessObservation,
    teardown_intent: Option<crate::native::session::close::TerminalTeardownIntent>,
    surface_present: impl FnOnce() -> Result<bool>,
) -> Result<super::TerminalCloseAuthority> {
    use super::TerminalCloseAuthority;
    use crate::native::process_identity::Gone;
    use anyhow::Context as _;
    use terminal::ownership::OwnerBinding;

    session.verify_managed_session(expected_session_id)?;
    // An interrupted owner teardown keeps its own completion rule below.
    if teardown_intent.is_none()
        && let Some(authority) = gone_owner(session, expected_session_id)?
    {
        return Ok(authority);
    }
    if matches!(process, ProcessObservation::Gone(Gone::Reused(_)))
        || observation.pid_alive(owner.pid)
        || owner.binding(expected_session_id) != OwnerBinding::Bound
        || !owner.control_record().is_complete()
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
    let absent = observation
        .evidence()
        .surface_absent(Some(owner), Box::new(surface_present))
        .context(
            "could not prove absence of the dead owner's exact terminal surface; no close was sent",
        )?;
    if absent {
        return Ok(TerminalCloseAuthority::Absent);
    }
    bail!(
        "the recorded native-session owner is no longer live; visible terminal cleanup is unverified and no close was sent"
    )
}

type CloseSurface<'a> = &'a mut dyn FnMut(&TerminalSession) -> Result<CloseOutcome>;

pub(in crate::native) enum RepairSurfaces<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Posix,
    #[cfg_attr(
        not(any(windows, all(test, any(target_os = "macos", target_os = "linux")))),
        allow(dead_code)
    )]
    Windows(CloseSurface<'a>),
    #[cfg(test)]
    WithoutSurface,
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    Unsupported,
}

pub(in crate::native) enum RepairSurface<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    Retain,
    Settle,
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    CloseOnWindows(WindowsClose<'a>),
}

impl<'a> RepairSurfaces<'a> {
    pub(in crate::native) fn decide(
        self,
        reader: &Reader,
        state: &SessionState,
        deadline: Instant,
    ) -> Result<RepairSurface<'a>> {
        match self {
            #[cfg(test)]
            Self::WithoutSurface => {
                use crate::native::session::CoreRecord;
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if reader
                    .record(CoreRecord::TerminalCloseIntent)
                    .bytes()?
                    .is_some()
                {
                    return Ok(RepairSurface::Retain);
                }
                for record in [CoreRecord::Terminal, CoreRecord::TerminalClosing] {
                    if reader.record(record).present()? {
                        bail!("a parity row records no surface");
                    }
                }
                Ok(RepairSurface::Settle)
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => {
                // Even an invalid intent is evidence of possible interrupted teardown.
                if reader
                    .record(crate::native::session::CoreRecord::TerminalCloseIntent)
                    .bytes()?
                    .is_some()
                    || has_retained_handle(reader, outlives_owner)?
                {
                    return Ok(RepairSurface::Retain);
                }
                Ok(RepairSurface::Settle)
            }
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            Self::Windows(close) => {
                // As on macOS and Linux, only an explicit close ends a terminal its core keeps.
                if has_retained_handle(reader, core_retains_finished)? {
                    return Ok(RepairSurface::Retain);
                }
                if matches!(state, SessionState::Exited | SessionState::Failed) {
                    close_remaining(deadline)?;
                    return Ok(RepairSurface::Settle);
                }
                Ok(RepairSurface::CloseOnWindows(WindowsClose { close }))
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            Self::Unsupported | Self::Windows(_) => {
                let _ = (reader, state, deadline);
                Ok(RepairSurface::Settle)
            }
        }
    }
}

// Preserve the active-before-claimed read order, including stopping at the first match.
// A damaged handle can be a surviving surface and must never be settled as absent.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn has_retained_handle(reader: &Reader, retains: fn(TerminalKind) -> bool) -> Result<bool> {
    use crate::native::session::{CoreRecord, record};
    use anyhow::Context as _;

    for record in [CoreRecord::Terminal, CoreRecord::TerminalClosing] {
        if let Some(bytes) = reader.record(record).bytes()? {
            let surface = record::terminal_bytes(&bytes)
                .with_context(|| format!("invalid retained terminal handle {}", record.name()))?;
            if retains(surface.kind) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(in crate::native) struct WindowsClose<'a> {
    close: CloseSurface<'a>,
}

impl WindowsClose<'_> {
    /// Check the claimed handle too. The earlier scan only decides whether Repair needs
    /// a close transaction. It conveys no proof over whatever handle that transaction claims.
    pub(in crate::native) fn close(
        &mut self,
        surface: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if core_retains_finished(surface.kind) {
            bail!(
                "Repair never ends a terminal its core keeps; close it with close-session --explicit"
            );
        }
        if surface.kind != TerminalKind::WindowsConsole {
            bail!("dead Windows native owner has a non-Windows terminal handle");
        }
        close_remaining(deadline)?;
        let outcome = (self.close)(surface)?;
        close_remaining(deadline)?;
        Ok(outcome)
    }

    pub(in crate::native) fn failure_context(&self) -> &'static str {
        "failed to close a Windows console whose native owner exited"
    }
}
