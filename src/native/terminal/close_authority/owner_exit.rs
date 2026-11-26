//! Surface decisions after the recorded owner has gone.
//!
//! One rule on every OS for a kind whose core keeps its terminals (Herdr; the owner's decision
//! of 2026-11-26, os-parity): Repair's dead-owner decision never ends, removes or reads such a
//! terminal and leaves its session for `close-session --explicit`. That close proves the
//! terminal by its own creation record once the owner is verified gone; the owner record
//! neither grants nor blocks it. A finished terminal is removed and an absent one settled
//! whatever the record says (exited or PID reused; bound, unbound or another session's;
//! complete or not). `core_kept` chooses between the close with a live owner and that close for
//! every owner record and returns evidence, never an action; the Herdr adapter's one granting
//! function decides from its own observation of the terminal. A close on the creation record
//! never ends a running terminal (the owner's decision of 2026-11-26): the person ends it or
//! waits for it to finish, and closes again. Only an explicit close acts on such a terminal; a
//! close that cleans up after another command does not.
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

/// The decision's evidence for a close on a terminal's creation record. Its fields are private
/// to this module, so only `core_kept` can establish it (and, in tests, `scripted`). A close on
/// the creation record removes a finished terminal and settles an absent one, and never ends a
/// running one (the owner's decision of 2026-11-26): nothing here permits that, and nothing
/// about the owner's process permits or refuses anything on this path. The evidence says why
/// the close runs, and, for the closed record's diagnosis only, the PID of this session's owner
/// when the OS reported its recorded process gone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
pub(in crate::native) struct CreationEvidence {
    cause: super::CloseCause,
    gone_owner: Option<u32>,
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
impl CreationEvidence {
    /// Whether `close-session --explicit` asked for this close.
    pub(in crate::native::terminal) fn explicit(self) -> bool {
        self.cause == super::CloseCause::Explicit
    }

    /// The reason the closed record carries when the session stored none: the end of this
    /// session's owner, as 0.4.49's Repair wrote it.
    pub(in crate::native::terminal) fn generated_reason(self) -> Option<String> {
        self.gone_owner
            .map(|pid| format!("native session process {pid} is no longer running"))
    }

    /// Evidence as a test states it, for tests of what the adapter grants from it.
    #[cfg(test)]
    pub(in crate::native) fn scripted(cause: super::CloseCause) -> Self {
        Self {
            cause,
            gone_owner: None,
        }
    }
}

/// What the decision found for a kind whose core keeps its terminals.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(super) enum CoreKept {
    /// No live owner of this session holds the surface: the close acts on the terminal's
    /// creation record, with this evidence.
    Created(super::CreationEvidence),
    /// A record bound to this session names a live, attested incarnation: the live-owner
    /// rules apply, with this record and attestation.
    LiveOwner(
        Box<(
            terminal::ownership::NativeSessionOwner,
            crate::native::session::owner::OwnerAttestation,
        )>,
    ),
}

/// The one place the close decision chooses, for a kind whose core keeps its terminals (Herdr),
/// between the close with a live owner and the close on the creation record, on every OS and
/// for every owner record. It returns evidence, never an action.
///
/// The deciding observation is the OS's attestation of the process that an owner record bound to
/// this session names, read in one typed read of the record:
/// - an attested live incarnation leads to the close with a live owner;
/// - when it cannot be made (the process cannot be inspected), the close refuses: nothing is
///   sent and the handle is retained, since the close with a live owner cannot be ruled out;
///   a record whose own fields conflict names nothing to inspect and takes the creation record;
/// - everything else takes the creation record: no record or one that cannot be decoded,
///   another session's or an unbound record (whose process is not inspected), a PID-only record
///   of a live process, a gone process.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(super) fn core_kept(
    observation: &dyn super::CloseObservation,
    expected_session_id: &str,
    session: &TerminalSession,
) -> Result<CoreKept> {
    use super::OwnerRead;
    use crate::native::process_identity::Attested;
    use crate::native::session::owner::record_conflicts;
    use terminal::ownership::OwnerBinding;

    session.verify_managed_session(expected_session_id)?;
    let cause = observation.cause();
    let created =
        |gone_owner: Option<u32>| Ok(CoreKept::Created(CreationEvidence { cause, gone_owner }));
    // A record that cannot be read is refused; one that is absent (a wrapper may still be
    // registering) or cannot be decoded names no owner.
    let owner = match observation.read_owner()? {
        OwnerRead::Absent | OwnerRead::Malformed => return created(None),
        OwnerRead::Recorded(owner) => *owner,
    };
    if owner.binding(expected_session_id) != OwnerBinding::Bound {
        return created(None);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let attestation = observation.posix_owner_live(&owner);
    #[cfg(windows)]
    let attestation = observation.observe_owner_record(&owner);
    match &attestation.process {
        ProcessObservation::Unknown(_) if record_conflicts(&owner) => created(None),
        ProcessObservation::Unknown(reason) => bail!("{reason}"),
        ProcessObservation::Live(Attested::Incarnation) => {
            Ok(CoreKept::LiveOwner(Box::new((owner, attestation))))
        }
        ProcessObservation::Live(Attested::PidOnly) => created(None),
        // For the diagnosis only.
        ProcessObservation::Gone(_) => created(Some(owner.pid)),
    }
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
            #[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
            Self::WithoutSurface => retain_or(reader, Retention::WithoutSurface, || {
                Ok(RepairSurface::Settle(Retention::WithoutSurface))
            }),
            // No surface can be retained here; the settlement checks the fixture again.
            #[cfg(all(test, not(any(target_os = "macos", target_os = "linux", windows))))]
            Self::WithoutSurface => Ok(RepairSurface::Settle(Retention::WithoutSurface)),
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
