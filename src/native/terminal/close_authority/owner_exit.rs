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
//! close that cleans up after another command does not. On macOS and Linux a saved teardown
//! intent must validate on its own fields. A live-owner replay requires the saved owner to
//! match the owner attested by `core_kept`. Without a verified live owner, the same
//! creation-record rule applies.
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
pub(in crate::native) fn core_retains_finished(kind: TerminalKind) -> bool {
    terminal::facts(kind).owner_survival == OwnerSurvival::CoreRetainsFinished
}

/// The decision's evidence for a close on a terminal's creation record. Its fields are private
/// to this module. `core_kept` constructs the decision's evidence. The adapter also has a
/// retained `surface_only` mapping, and tests can supply `scripted` evidence. A close on the creation record
/// removes a finished terminal and settles an absent one, and never ends a running one (the
/// owner's decision of 2026-11-26): nothing here permits that, and nothing about the owner
/// permits or refuses anything on this path. The evidence says why the close runs, and, for
/// messages only: the PID of this session's owner when the OS reported its recorded process
/// gone (the closed record's diagnosis), and why a live owner could not be verified when the
/// owner record or its process could not be read (the refusal of a running terminal).
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
pub(in crate::native) struct CreationEvidence {
    source: Source,
    gone_owner: Option<u32>,
    unverified: Option<String>,
}

/// Which decision sent the close to the creation record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
enum Source {
    /// `core_kept`, for a close run for this cause.
    Decided(super::CloseCause),
    /// The adapter was given a `SurfaceOnly` authority.
    SurfaceOnly,
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
impl CreationEvidence {
    /// The retained adapter mapping admits `SurfaceOnly` for either close cause.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native::terminal) fn surface_only() -> Self {
        Self {
            source: Source::SurfaceOnly,
            gone_owner: None,
            unverified: None,
        }
    }

    /// Whether `close-session --explicit` asked for this close, as `core_kept` found it.
    pub(in crate::native::terminal) fn explicit(&self) -> bool {
        self.source == Source::Decided(super::CloseCause::Explicit)
    }

    /// Admission for an explicit close or the retained `SurfaceOnly` mapping.
    pub(in crate::native::terminal) fn admitted(&self) -> bool {
        self.explicit() || self.source == Source::SurfaceOnly
    }

    /// The reason the closed record carries when the session stored none: the end of this
    /// session's owner, as 0.4.49's Repair wrote it.
    pub(in crate::native::terminal) fn generated_reason(&self) -> Option<String> {
        self.gone_owner
            .map(|pid| format!("native session process {pid} is no longer running"))
    }

    /// Why a live owner of this session could not be verified, when the owner record or its
    /// process could not be read.
    pub(in crate::native::terminal) fn unverified(&self) -> Option<&str> {
        self.unverified.as_deref()
    }

    /// Evidence as a test states it, for tests of what the adapter grants from it.
    #[cfg(test)]
    pub(in crate::native) fn scripted(cause: super::CloseCause) -> Self {
        Self {
            source: Source::Decided(cause),
            gone_owner: None,
            unverified: None,
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
/// The owner record answers exactly one question: is there an attested live incarnation of this
/// session's own owner, from a record whose fields do not contradict each other (no other
/// platform's incarnation fields)? If so, the close with a live owner applies, unchanged. In
/// every other case the close on the creation record applies, which never ends a running
/// terminal: a record that cannot be read, none, one that cannot be decoded, another session's
/// or an unbound one (whose process is not inspected), a contradictory one, a process that
/// cannot be inspected, a PID-only record of a live process, a gone process. Nothing about the
/// owner refuses a finished or absent terminal.
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
    let created = |gone_owner: Option<u32>, unverified: Option<String>| {
        Ok(CoreKept::Created(CreationEvidence {
            source: Source::Decided(cause),
            gone_owner,
            unverified,
        }))
    };
    let owner = match observation.read_owner() {
        Err(error) => {
            return created(
                None,
                Some(format!("its owner record could not be read: {error:#}")),
            );
        }
        // No record (a wrapper may still be registering), or one that cannot be decoded.
        Ok(OwnerRead::Absent | OwnerRead::Malformed) => return created(None, None),
        Ok(OwnerRead::Recorded(owner)) => *owner,
    };
    if owner.binding(expected_session_id) != OwnerBinding::Bound || record_conflicts(&owner) {
        return created(None, None);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let attestation = observation.posix_owner_live(&owner);
    #[cfg(windows)]
    let attestation = observation.observe_owner_record(&owner);
    // The control read after a matching birth found the process gone (macOS and Linux).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if matches!(
        attestation.control,
        crate::native::session::owner::ControlAttestation::Exited
    ) {
        return created(Some(owner.pid), None);
    }
    match &attestation.process {
        ProcessObservation::Live(Attested::Incarnation) => {
            Ok(CoreKept::LiveOwner(Box::new((owner, attestation))))
        }
        ProcessObservation::Unknown(reason) => created(
            None,
            Some(format!(
                "its owner's process could not be inspected: {reason}"
            )),
        ),
        ProcessObservation::Live(Attested::PidOnly) => created(None, None),
        // For the diagnosis only.
        ProcessObservation::Gone(_) => created(Some(owner.pid), None),
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
    /// Settle from the Records after `Retention` rechecks under the lifecycle lock.
    /// Standalone Repair makes its initial choice before taking that lock.
    Settle(Retention),
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux", windows)),
        allow(dead_code)
    )]
    CloseOnWindows(WindowsClose<'a>),
}

/// What a dead owner's Repair keeps for an explicit close. Standalone Repair scans
/// before taking the lifecycle lock and rechecks under the lock before settlement.
/// Its first scan can miss a handle that a concurrent explicit close later restores.
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
    pub(in crate::native) fn retains(
        self,
        reader: &Reader,
        active: impl FnOnce() -> Result<Option<TerminalKind>>,
        claimed: impl FnOnce() -> Result<Option<TerminalKind>>,
    ) -> Result<bool> {
        #[cfg(all(windows, not(test)))]
        let _ = reader; // Windows retention uses only the two handle kinds.
        let has_kind = |retains: fn(TerminalKind) -> bool| -> Result<bool> {
            Ok(active()?.is_some_and(retains) || claimed()?.is_some_and(retains))
        };
        match self {
            // Even an invalid intent is evidence of possible interrupted teardown.
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => Ok(reader
                .record(crate::native::session::CoreRecord::TerminalCloseIntent)
                .bytes()?
                .is_some()
                || has_kind(outlives_owner)?),
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            Self::CoreKept => has_kind(core_retains_finished),
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
                let _ = (reader, has_kind);
                Ok(false)
            }
        }
    }
}

impl<'a> RepairSurfaces<'a> {
    pub(in crate::native) fn decide(
        self,
        state: &SessionState,
        deadline: Instant,
        retains: impl FnOnce(Retention) -> Result<bool>,
    ) -> Result<RepairSurface<'a>> {
        match self {
            #[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
            Self::WithoutSurface => retain_or(retains, Retention::WithoutSurface, || {
                Ok(RepairSurface::Settle(Retention::WithoutSurface))
            }),
            // No surface can be retained here; the settlement checks the fixture again.
            #[cfg(all(test, not(any(target_os = "macos", target_os = "linux", windows))))]
            Self::WithoutSurface => Ok(RepairSurface::Settle(Retention::WithoutSurface)),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::Posix => retain_or(retains, Retention::Posix, || {
                Ok(RepairSurface::Settle(Retention::Posix))
            }),
            // As on macOS and Linux, only an explicit close ends a terminal its core keeps.
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            Self::Windows(close) => retain_or(retains, Retention::CoreKept, || {
                if matches!(state, SessionState::Exited | SessionState::Failed) {
                    close_remaining(deadline)?;
                    return Ok(RepairSurface::Settle(Retention::CoreKept));
                }
                Ok(RepairSurface::CloseOnWindows(WindowsClose { close }))
            }),
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            Self::Unsupported | Self::Windows(_) => {
                let _ = (retains, state, deadline);
                Ok(RepairSurface::Settle(Retention::Nothing))
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn retain_or<'a>(
    retains: impl FnOnce(Retention) -> Result<bool>,
    retention: Retention,
    otherwise: impl FnOnce() -> Result<RepairSurface<'a>>,
) -> Result<RepairSurface<'a>> {
    if retains(retention)? {
        return Ok(RepairSurface::Retain);
    }
    otherwise()
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
