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
//! | `Created`: no live owner of this session; admitted only for an explicit close | Recorded identity and launch label, from a handle whose creation record is whole | Yes | Only when this session's owner is positively gone and the program is observed with its recorded birth |
//!
//! A `not_found` from the recorded core settles a `Created` close only after the handle's
//! creation record was validated (`Recorded::open` with `Requirement::Created`).
use super::identity::{Launch, Recorded};
use super::{Core, Observed, State};
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
        first: &'a Observed,
    },
    /// No live owner of this session: the terminal's creation record decides, with the
    /// decision's admitted evidence.
    Created {
        recorded: &'a Recorded<'a>,
        evidence: Admitted,
    },
}

/// Evidence admitted for a close on the creation record: only an explicit close's.
#[derive(Clone, Copy)]
pub(super) struct Admitted(CreationEvidence);

/// Admits the decision's evidence before anything is read from the core: a cleanup after
/// another command (a refused reopen) acts on no terminal that no live owner holds, and settles
/// no absent one either.
pub(super) fn admit(evidence: CreationEvidence) -> Result<Admitted> {
    if !evidence.explicit() {
        bail!(
            "no live owner of this session holds its Herdr terminal; only close-session --explicit ends, removes or settles it, so nothing was sent and the handle is retained"
        );
    }
    Ok(Admitted(evidence))
}

/// The authority to send one `terminal.kill` to the terminal `grant` observed, on its core.
/// Only `grant` constructs it, and only `Linked::kill` consumes it.
pub(super) struct Kill {
    core_id: String,
    address: String,
}

/// The authority to send one `terminal.rm` to the terminal `grant` observed, on its core.
/// Only `grant` constructs it, and only `Linked::remove` consumes it.
pub(super) struct Remove {
    core_id: String,
    address: String,
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

/// The only function that grants `terminal.kill` or `terminal.rm`, for the terminal observed on
/// the connection to core `core_id`.
pub(super) fn grant(
    core: &dyn Core,
    basis: &Basis<'_>,
    core_id: &str,
    observed: &Observed,
    killed: bool,
) -> Result<Grant> {
    match basis {
        Basis::Owned(recorded) => recorded.check(core, observed)?,
        Basis::Launch { launch, first } => launch.check_cleanup(first, observed)?,
        Basis::Created { recorded, .. } => recorded.check_creation_record(core, observed)?,
    }
    match observed.state {
        State::Finished => Ok(Grant::Remove(Remove {
            core_id: core_id.to_owned(),
            address: observed.address.clone(),
        })),
        State::Live if killed => Ok(Grant::Wait),
        State::Live => {
            if let Basis::Created { recorded, evidence } = basis {
                if !evidence.0.owner_gone() {
                    bail!(
                        "Herdr terminal {} is still running and nothing shows that this session's owner is gone, so TAB does not end it; nothing was sent and the handle is retained",
                        observed.address
                    );
                }
                recorded.program_observed(core, observed)?;
            }
            Ok(Grant::Kill(Kill {
                core_id: core_id.to_owned(),
                address: observed.address.clone(),
            }))
        }
    }
}
