//! Surface decisions after the recorded owner has gone.
//!
//! One rule on every OS for a kind whose core keeps its terminals (Herdr; the owner's decision
//! of 2026-11-26, os-parity): Repair's dead-owner decision never ends, removes or reads such a
//! terminal and leaves its session for `close-session --explicit`. That close proves the
//! terminal by its own creation record once the owner is verified gone; the owner record
//! neither grants nor blocks it. A finished terminal is removed and an absent one settled
//! whatever the record says (exited or PID reused; bound, unbound or another session's;
//! complete or not). A running terminal is ended only when this session's owner is positively
//! gone: a record bound to this session that names an incarnation the OS observed exited or
//! reused. A foreign, unbound or PID-only record cannot show that, so its running terminal is
//! retained until the user ends it on its core. The adapter then ends it only after observing
//! its program with the recorded birth. Only an explicit close acts on a gone owner's
//! terminal; a close that cleans up after another command does not.
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

/// Whether the owner of this session is positively gone: the record is bound to this session
/// and names an incarnation, and the OS observed that incarnation exited or its PID reused. A
/// record of another session, an unbound one or a PID-only one can be observed gone without
/// showing that this session has ended.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(super) fn this_sessions_owner_gone(
    owner: &terminal::ownership::NativeSessionOwner,
    expected_session_id: &str,
    process: &ProcessObservation,
) -> bool {
    owner.binding(expected_session_id) == terminal::ownership::OwnerBinding::Bound
        && crate::native::session::owner::records_incarnation(owner)
        && matches!(process, ProcessObservation::Gone(_))
}

/// A startup surface with no live owner: a failed start whose owner is gone, or one that never
/// recorded an owner (`owner_gone` is then true). A kind whose core keeps its terminals closes
/// on the creation record, launch label and program birth included; the other kinds keep the
/// surface-only close.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(super) fn startup_surface(
    session: &TerminalSession,
    owner_gone: bool,
) -> super::TerminalCloseAuthority {
    if !core_retains_finished(session.kind) {
        return super::TerminalCloseAuthority::SurfaceOnly;
    }
    super::TerminalCloseAuthority::CreatedSurface(if owner_gone {
        super::Running::End
    } else {
        super::Running::Keep
    })
}

/// The authority over a surface whose recorded owner is observed gone, the same on every OS: a
/// kind whose core keeps its terminals is closed on the terminal's creation record, and only
/// by an explicit close. `None` leaves the decision to the platform's rule for the other kinds.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn gone_owner(
    observation: &dyn super::CloseObservation,
    session: &TerminalSession,
    expected_session_id: &str,
    owner: &terminal::ownership::NativeSessionOwner,
    process: &ProcessObservation,
) -> Result<Option<super::TerminalCloseAuthority>> {
    if !core_retains_finished(session.kind) {
        return Ok(None);
    }
    session.verify_managed_session(expected_session_id)?;
    if observation.cause() != super::CloseCause::Explicit {
        bail!(
            "the session's native owner is gone; only close-session --explicit ends or removes its {} terminal, so nothing was sent and the handle is retained",
            session.kind.display_name()
        );
    }
    Ok(Some(super::TerminalCloseAuthority::CreatedSurface(
        if this_sessions_owner_gone(owner, expected_session_id, process) {
            super::Running::End
        } else {
            super::Running::Keep
        },
    )))
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
    let process = observation.observe_owner_record(&owner).process;
    if !matches!(process, ProcessObservation::Gone(_)) {
        return Ok(None);
    }
    gone_owner(observation, session, expected_session_id, &owner, &process)
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
        && let Some(authority) =
            gone_owner(observation, session, expected_session_id, owner, &process)?
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
    /// Settle from the Records, after `Retention` finds nothing to keep again under the
    /// lifecycle lock: the scan that chose to settle ran without it.
    Settle(Retention),
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    CloseOnWindows(WindowsClose<'a>),
}

/// What a dead owner's Repair keeps for an explicit close instead of settling it. The scan
/// runs once without the lifecycle lock to choose, and again under that lock before a
/// settlement: an explicit close holds the lock from its handle claim to its restore, so the
/// first scan can miss a handle that the close then restores.
#[derive(Clone, Copy)]
pub(in crate::native) enum Retention {
    /// macOS and Linux: a close intent, even an invalid one, and a handle of a kind whose
    /// surface can outlive its owner.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Posix,
    /// Windows: a handle of a kind whose core keeps its terminals.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    CoreKept,
    /// The parity fixtures record no surface; only a POSIX close intent is kept.
    #[cfg(test)]
    WithoutSurface,
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    Nothing,
}

impl Retention {
    pub(in crate::native) fn retains(self, reader: &Reader) -> Result<bool> {
        match self {
            // Even an invalid intent is evidence of possible interrupted teardown.
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => Ok(reader
                .record(crate::native::session::CoreRecord::TerminalCloseIntent)
                .bytes()?
                .is_some()
                || has_retained_handle(reader, outlives_owner)?),
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            Self::CoreKept => has_retained_handle(reader, core_retains_finished),
            #[cfg(test)]
            Self::WithoutSurface => {
                use crate::native::session::CoreRecord;
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if reader
                    .record(CoreRecord::TerminalCloseIntent)
                    .bytes()?
                    .is_some()
                {
                    return Ok(true);
                }
                for record in [CoreRecord::Terminal, CoreRecord::TerminalClosing] {
                    if reader.record(record).present()? {
                        bail!("a parity row records no surface");
                    }
                }
                Ok(false)
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            Self::Nothing => {
                let _ = reader;
                Ok(false)
            }
        }
    }
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
            Self::WithoutSurface => retain_or(reader, Retention::WithoutSurface, || {
                Ok(RepairSurface::Settle(Retention::WithoutSurface))
            }),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => retain_or(reader, Retention::Posix, || {
                Ok(RepairSurface::Settle(Retention::Posix))
            }),
            // As on macOS and Linux, only an explicit close ends a terminal its core keeps.
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            Self::Windows(close) => retain_or(reader, Retention::CoreKept, || {
                if matches!(state, SessionState::Exited | SessionState::Failed) {
                    close_remaining(deadline)?;
                    return Ok(RepairSurface::Settle(Retention::CoreKept));
                }
                Ok(RepairSurface::CloseOnWindows(WindowsClose { close }))
            }),
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            Self::Unsupported | Self::Windows(_) => {
                let _ = (reader, state, deadline);
                Ok(RepairSurface::Settle(Retention::Nothing))
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn retain_or<'a>(
    reader: &Reader,
    retention: Retention,
    otherwise: impl FnOnce() -> Result<RepairSurface<'a>>,
) -> Result<RepairSurface<'a>> {
    if retention.retains(reader)? {
        return Ok(RepairSurface::Retain);
    }
    otherwise()
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
