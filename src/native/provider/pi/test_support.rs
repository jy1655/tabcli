use super::{PendingPiTurn, install_pending_turn};
use crate::native::session::{Store, turn};
use std::path::Path;

pub(super) fn claim_pending_turn(directory: &Path) -> PendingPiTurn {
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    install_pending_turn(directory, &token).unwrap()
}

pub(super) fn marked(message: &str, pending: &PendingPiTurn) -> String {
    format!("{message}\n{}", pending.marker)
}
