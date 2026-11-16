//! Pending launch: whether a launch receipt is still the pending launch of a caller's Claim.
//! One judgment for every launch admission check (the binding waits, the launch hosts, the
//! wrapper's admission and the spawn fence). It takes no lock: each caller holds the locks
//! it needs, reads the receipt itself first, chooses its deadline policy, and maps the
//! failing fact to its own error text. A check without the lifecycle lock is advisory; the
//! locked spawn fence is final.
use super::{Phase, Record};
use crate::native::session::primitives::unix_ms;
#[cfg(any(target_os = "macos", test))]
use crate::native::session::{CoreRecord, record};
use crate::native::session::{Reader, SessionState, turn};
#[cfg(any(target_os = "macos", test))]
use crate::native::terminal::TerminalSession;
#[cfg(any(target_os = "macos", test))]
use anyhow::Context;
use anyhow::Result;

/// The caller's deadline policy. The status and the tombstone are read whatever the time.
pub(in crate::native) enum Expiry<'a> {
    /// No deadline: a later check under the lifecycle lock judges it.
    Unchecked,
    /// The receipt's recorded deadline.
    Receipt,
    /// The caller's own budget on its clock, then the receipt's recorded deadline. Only the
    /// macOS binding wait has a budget of its own.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    BudgetOrReceipt(&'a mut dyn FnMut() -> bool),
}

/// The first fact, in the judgment's order, that ends the pending launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum NotPending {
    /// The Session is no longer launching.
    State(SessionState),
    /// Close has written the Session's tombstone, while the status may still say launching.
    Closed,
    /// The receipt names another Claim than the caller's.
    ReceiptClaim,
    /// The launch was already attempted.
    Phase(Phase),
    /// The deadline passed under the caller's policy.
    Expired,
    /// The caller's Claim is no longer the current one.
    Claim,
}

/// The Session is launching and no close tombstone ends it: the status is read first, then
/// the tombstone, which must be a regular file. A launch without a receipt is judged on
/// these two facts alone.
pub(in crate::native) fn launching(reader: &Reader) -> Result<std::result::Result<(), NotPending>> {
    let state = reader.status()?.state;
    if state != SessionState::Launching {
        return Ok(Err(NotPending::State(state)));
    }
    if reader.closed_if_present()?.is_some() {
        return Ok(Err(NotPending::Closed));
    }
    Ok(Ok(()))
}

/// Whether `receipt`, which the caller has read, is the Pending launch of the Claim `token`.
/// One order: the status (read even after expiry), the tombstone, the receipt's Claim, its
/// phase and its deadline under `expiry`, and last the current Claim, which is read only
/// when every earlier fact holds. The first failing fact is returned; a record that cannot
/// be read is an error.
pub(in crate::native) fn judge(
    reader: &Reader,
    receipt: &Record,
    token: &str,
    expiry: Expiry<'_>,
) -> Result<std::result::Result<(), NotPending>> {
    if let Err(not_pending) = launching(reader)? {
        return Ok(Err(not_pending));
    }
    if receipt.claim_token != token {
        return Ok(Err(NotPending::ReceiptClaim));
    }
    if receipt.phase != Phase::Pending {
        return Ok(Err(NotPending::Phase(receipt.phase)));
    }
    let receipt_expired = || unix_ms() >= receipt.deadline_unix_ms;
    let expired = match expiry {
        Expiry::Unchecked => false,
        Expiry::Receipt => receipt_expired(),
        Expiry::BudgetOrReceipt(budget_spent) => budget_spent() || receipt_expired(),
    };
    if expired {
        return Ok(Err(NotPending::Expired));
    }
    if turn::current_claim_token(reader)?.as_deref() != Some(token) {
        return Ok(Err(NotPending::Claim));
    }
    Ok(Ok(()))
}

/// The bound surface of this Session: the terminal handle, decoded, naming `session_id`.
/// `None` while no surface is bound. An undecodable handle carries the caller's `invalid`
/// context; a handle of another Session fails with the binding's own error. The caller
/// judges the kind and whether the surface names its host.
#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn bound_surface<C>(
    reader: &Reader,
    session_id: &str,
    invalid: impl FnOnce() -> C,
) -> Result<Option<TerminalSession>>
where
    C: std::fmt::Display + Send + Sync + 'static,
{
    let Some(text) = reader.record(CoreRecord::Terminal).text()? else {
        return Ok(None);
    };
    let surface = record::terminal(&text).with_context(invalid)?;
    surface.verify_managed_session(session_id)?;
    Ok(Some(surface))
}

#[cfg(test)]
mod tests;
