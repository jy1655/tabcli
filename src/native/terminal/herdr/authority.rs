//! The one place a Herdr close gains the authority to act on a terminal. `grant` mints the only
//! values that let `terminal.kill` and `terminal.rm` be sent; their constructors are private to
//! this module. It decides from the basis of the close (what holds or proves the terminal) and
//! from the observation the action is about to act on, and grants nothing that this evidence
//! does not establish:
//!
//! | Basis | Identity checked on every read | `terminal.rm` (finished) | `terminal.kill` (running) |
//! | --- | --- | --- | --- |
//! | `Owned`: a live, attested owner of this session, or the launcher that bound the terminal | Recorded identity | Yes | Yes |
//! | `Launch`: the launch that created the terminal, before any binding | The launch's argv, label, name, creation time and PID | Yes | Yes |
//! | `Created`: no live owner of this session; admitted for an explicit close, or for a close the decision left to the surface alone | Recorded identity and launch label, from a handle whose creation record is whole | Yes | Never |
//!
//! A `not_found` from the recorded core settles a `Created` close only after the handle's
//! creation record was validated (`Recorded::open` with `Requirement::Created`). A close on the
//! creation record never ends a running terminal, whatever is known about the session's owner
//! (the owner's decision of 2026-11-26): it refuses, and the person ends the terminal or waits
//! for it to finish before closing again.
use super::identity::{Launch, Recorded};
use super::linked::Seen;
use super::{Core, State};
use crate::native::terminal::close_authority::CreationEvidence;
use anyhow::{Result, bail};

/// What holds or proves the terminal a close acts on.
pub(super) enum Basis<'a> {
    /// A live, attested owner of this session holds the surface (the ordinary close), or the
    /// launcher that created and bound the terminal removes it.
    Owned(&'a Recorded<'a>),
    /// The launch that created the terminal removes it before any binding.
    Launch {
        launch: &'a Launch<'a>,
        first: &'a Seen,
    },
    /// No live owner of this session: the terminal's creation record decides, for an admitted
    /// close.
    Created {
        recorded: &'a Recorded<'a>,
        admitted: &'a Admitted,
    },
}

/// A close on the creation record that `admit` let through: an explicit close, or a close that
/// the decision left to the surface alone (`CreationEvidence::surface_only`). It keeps why
/// a live owner could not be verified, for the refusal of a running terminal and for
/// `explain`, which the close attaches once around everything it does after admission.
pub(super) struct Admitted {
    unverified: Option<String>,
}

/// The refusal of a running terminal on the creation record (`Recorded::still_running`). It
/// states why no live owner was verified in its own text, so `Admitted::explain` adds nothing
/// to it.
#[derive(Debug)]
struct StillRunning(String);

impl std::fmt::Display for StillRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StillRunning {}

/// Admits the decision's evidence before anything is read from the core: a cleanup after
/// another command (a refused reopen) that `core_kept` sent here acts on no terminal that no live
/// owner holds, and settles no absent one either. A close that the decision left to the surface
/// alone is admitted whatever its cause, because 0.4.49 acted on the surface for it.
pub(super) fn admit(evidence: CreationEvidence) -> Result<Admitted> {
    if !evidence.admitted() {
        let why = evidence
            .unverified()
            .map(|why| format!(" ({why})"))
            .unwrap_or_default();
        bail!(
            "TAB could not verify a live owner for this session{why}; without one, only close-session --explicit removes or settles its Herdr terminal, so nothing was sent and the handle is retained"
        );
    }
    Ok(Admitted {
        unverified: evidence.unverified().map(str::to_owned),
    })
}

/// The authority to send one `terminal.kill` to the terminal `grant` observed, on its core.
/// Only `grant` constructs it, and only `Linked::end_program` consumes it.
pub(super) struct Kill {
    core_id: String,
    address: String,
}

/// The authority to send one `terminal.rm` to the terminal `grant` observed, on its core.
/// Only `grant` constructs it, and only `Linked::remove_confirmed` consumes it.
pub(super) struct Remove {
    core_id: String,
    address: String,
}

impl Admitted {
    /// A refusal on the creation record keeps why a live owner could not be verified, when the
    /// owner record or its process could not be read, beside its own recovery instructions.
    /// The close attaches it once, around the whole admitted operation; the refusal of a running
    /// terminal already says why.
    pub(super) fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        match &self.unverified {
            Some(why) if error.downcast_ref::<StillRunning>().is_none() => error.context(format!(
                "TAB could not verify a live owner for this session ({why})"
            )),
            _ => error,
        }
    }
}

impl Kill {
    pub(super) fn core_id(&self) -> &str {
        &self.core_id
    }

    pub(super) fn address(&self) -> &str {
        &self.address
    }
}

impl Remove {
    pub(super) fn core_id(&self) -> &str {
        &self.core_id
    }

    pub(super) fn address(&self) -> &str {
        &self.address
    }
}

pub(super) enum Grant {
    Kill(Kill),
    Remove(Remove),
    /// The program was ended by this close and the core still reports it running.
    Wait,
}

/// The only function that grants `terminal.kill` or `terminal.rm`. It mints from `seen`, an
/// observation a verified connection produced, so the grant names the core that reported the
/// terminal and that terminal's address; it refuses an observation from any core, or of any
/// address, other than the one the basis is about.
pub(super) fn grant(
    core: &dyn Core,
    basis: &Basis<'_>,
    seen: &Seen,
    killed: bool,
) -> Result<Grant> {
    let (expected_core, expected_address) = match basis {
        Basis::Owned(recorded) | Basis::Created { recorded, .. } => {
            (recorded.core_id(), recorded.address())
        }
        Basis::Launch { first, .. } => (first.core_id(), first.observed().address.as_str()),
    };
    let observed = seen.observed();
    if seen.core_id() != expected_core {
        bail!(
            "Herdr terminal {} was observed on Herdr Core {}, not on the core {expected_core} this close acts on; nothing was sent",
            observed.address,
            seen.core_id()
        );
    }
    if observed.address != expected_address {
        bail!(
            "Herdr terminal {} is not the terminal {expected_address} this close acts on; nothing was sent",
            observed.address
        );
    }
    match basis {
        Basis::Owned(recorded) => recorded.check(core, observed)?,
        Basis::Launch { launch, first } => launch.check_cleanup(first.observed(), observed)?,
        Basis::Created { recorded, .. } => recorded.check_creation_record(core, observed)?,
    }
    let core_id = seen.core_id();
    match observed.state {
        State::Finished => Ok(Grant::Remove(Remove {
            core_id: core_id.to_owned(),
            address: observed.address.clone(),
        })),
        State::Live if killed => Ok(Grant::Wait),
        State::Live => {
            if let Basis::Created { recorded, admitted } = basis {
                return Err(StillRunning(
                    recorded.still_running(&observed.address, admitted.unverified.as_deref()),
                )
                .into());
            }
            Ok(Grant::Kill(Kill {
                core_id: core_id.to_owned(),
                address: observed.address.clone(),
            }))
        }
    }
}
