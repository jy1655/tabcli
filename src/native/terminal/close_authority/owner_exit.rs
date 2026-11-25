//! Surface decisions after the recorded owner has gone. Repair retains POSIX surfaces for
//! explicit close. Windows Repair can close through the surface adapter's independent proof.
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
    terminal::facts(kind).owner_survival != OwnerSurvival::EndsWithOwner
}

pub(in crate::native::terminal) fn core_retains_finished(kind: TerminalKind) -> bool {
    terminal::facts(kind).owner_survival == OwnerSurvival::CoreRetainsFinished
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
    if core_retains_finished(session.kind) {
        return Ok(TerminalCloseAuthority::RemoveEnded);
    }
    bail!(
        "the recorded native-session owner is no longer live; visible terminal cleanup is unverified and no close was sent"
    )
}

type CloseSurface<'a> = &'a mut dyn FnMut(&TerminalSession) -> Result<CloseOutcome>;

pub(in crate::native) enum RepairSurfaces<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Posix,
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    Windows(CloseSurface<'a>),
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    Unsupported,
}

pub(in crate::native) enum RepairSurface<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Retain,
    Settle,
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    CloseOnWindows(WindowsClose<'a>),
}

impl<'a> RepairSurfaces<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native) fn decide(
        self,
        reader: &Reader,
        state: &SessionState,
        deadline: Instant,
    ) -> Result<RepairSurface<'a>> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => {
                // Even an invalid intent is evidence of possible interrupted teardown.
                if reader
                    .record(crate::native::session::CoreRecord::TerminalCloseIntent)
                    .bytes()?
                    .is_some()
                    || has_retained_handle(
                        reader,
                        outlives_owner,
                        "invalid retained terminal handle",
                    )?
                {
                    return Ok(RepairSurface::Retain);
                }
                Ok(RepairSurface::Settle)
            }
            Self::Windows(close) => {
                let core_retains_finished = has_retained_handle(
                    reader,
                    core_retains_finished,
                    "cannot identify the retained terminal handle",
                )?;
                if matches!(state, SessionState::Exited | SessionState::Failed)
                    && !core_retains_finished
                {
                    close_remaining(deadline)?;
                    return Ok(RepairSurface::Settle);
                }
                Ok(RepairSurface::CloseOnWindows(WindowsClose {
                    close,
                    core_retains_finished,
                }))
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub(in crate::native) fn decide(
        self,
        _reader: &Reader,
        _state: &SessionState,
        _deadline: Instant,
    ) -> Result<RepairSurface<'a>> {
        Ok(RepairSurface::Settle)
    }
}

// Preserve the active-before-claimed read order, including stopping at the first match.
// A damaged handle can be a surviving surface and must never be settled as absent.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn has_retained_handle(
    reader: &Reader,
    retains: fn(TerminalKind) -> bool,
    context: &str,
) -> Result<bool> {
    use crate::native::session::{CoreRecord, record};
    use anyhow::Context as _;

    for record in [CoreRecord::Terminal, CoreRecord::TerminalClosing] {
        if let Some(bytes) = reader.record(record).bytes()? {
            let surface = record::terminal_bytes(&bytes)
                .with_context(|| format!("{context} {}", record.name()))?;
            if retains(surface.kind) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(in crate::native) struct WindowsClose<'a> {
    close: CloseSurface<'a>,
    core_retains_finished: bool,
}

impl WindowsClose<'_> {
    /// Check the claimed handle too. The earlier scan only decides whether Repair needs
    /// a close transaction. It conveys no proof over whatever handle that transaction claims.
    pub(in crate::native) fn close(
        &mut self,
        surface: &TerminalSession,
        deadline: Instant,
    ) -> Result<CloseOutcome> {
        if !core_retains_finished(surface.kind) && surface.kind != TerminalKind::WindowsConsole {
            bail!("dead Windows native owner has a non-Windows terminal handle");
        }
        close_remaining(deadline)?;
        let outcome = (self.close)(surface)?;
        close_remaining(deadline)?;
        Ok(outcome)
    }

    pub(in crate::native) fn failure_context(&self) -> &'static str {
        if self.core_retains_finished {
            "failed to close a Herdr terminal whose native owner exited; its handle is retained"
        } else {
            "failed to close a Windows console whose native owner exited"
        }
    }
}
