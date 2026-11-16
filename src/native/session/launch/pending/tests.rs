use super::*;
use crate::native::session::Store;
use crate::native::session::launch::test_support::{
    Change::{self, *},
    apply, claim_error, pending_launch, status_error, tombstone_error,
};
use std::{cell::Cell, time::Duration};

#[derive(Clone, Copy, Debug)]
enum Policy {
    Unchecked,
    Receipt,
    Budget { spent: bool },
}

#[derive(Clone, Debug)]
enum Outcome {
    Pending,
    Not(NotPending),
    StatusError,
    TombstoneError,
    ClaimError,
}

fn directory(changes: &[Change]) -> (tempfile::TempDir, Store, String) {
    let fixture = pending_launch("session-", &std::env::temp_dir(), Duration::from_secs(30));
    apply(&fixture.1, changes);
    fixture
}

fn expected(reader: &Reader, outcome: &Outcome) -> std::result::Result<(), String> {
    match outcome {
        Outcome::Pending => Ok(()),
        Outcome::Not(not_pending) => Err(format!("{not_pending:?}")),
        Outcome::StatusError => Err(status_error(reader)),
        Outcome::TombstoneError => Err(tombstone_error(reader)),
        Outcome::ClaimError => Err(claim_error(reader)),
    }
}

fn judged(
    reader: &Reader,
    token: &str,
    policy: Policy,
    budget_reads: &Cell<usize>,
) -> std::result::Result<(), String> {
    let budget_spent = |spent: bool| {
        budget_reads.set(budget_reads.get() + 1);
        spent
    };
    let receipt = super::super::read(reader).unwrap().unwrap();
    let judged = match policy {
        Policy::Unchecked => judge(reader, &receipt, token, Expiry::Unchecked),
        Policy::Receipt => judge(reader, &receipt, token, Expiry::Receipt),
        Policy::Budget { spent } => judge(
            reader,
            &receipt,
            token,
            Expiry::BudgetOrReceipt(&mut || budget_spent(spent)),
        ),
    };
    match judged {
        Ok(Ok(())) => Ok(()),
        Ok(Err(not_pending)) => Err(format!("{not_pending:?}")),
        Err(error) => Err(format!("{error:#}")),
    }
}

// (case, changes, whether the caller's Claim is the fixture's own, outcome per policy)
type Case = (&'static str, Vec<Change>, bool, Vec<(Policy, Outcome)>);

#[test]
fn judgment_returns_the_first_failing_fact_in_one_order() {
    use NotPending::*;
    use Outcome::{ClaimError, Not, StatusError, TombstoneError};
    const ALL: [Policy; 4] = [
        Policy::Unchecked,
        Policy::Receipt,
        Policy::Budget { spent: false },
        Policy::Budget { spent: true },
    ];
    let unexpired = |outcome: fn() -> Outcome| {
        ALL.map(|policy| {
            (
                policy,
                match policy {
                    Policy::Budget { spent: true } => Not(Expired),
                    _ => outcome(),
                },
            )
        })
        .to_vec()
    };
    let every = |outcome: fn() -> Outcome| ALL.map(|policy| (policy, outcome())).to_vec();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases: Vec<Case> = vec![
        ("pending", vec![], true, unexpired(|| Outcome::Pending)),
        (
            "status closed",
            vec![StatusClosed],
            true,
            every(|| Not(State(SessionState::Closed))),
        ),
        (
            "status failed",
            vec![StatusFailed],
            true,
            every(|| Not(State(SessionState::Failed))),
        ),
        (
            "status damaged",
            vec![StatusDamaged],
            true,
            every(|| StatusError),
        ),
        ("tombstone", vec![Tombstone], true, every(|| Not(Closed))),
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            true,
            every(|| TombstoneError),
        ),
        (
            "tombstone directory",
            vec![TombstoneDirectory],
            true,
            every(|| TombstoneError),
        ),
        (
            "another caller Claim",
            vec![],
            false,
            every(|| Not(ReceiptClaim)),
        ),
        (
            "receipt of another Claim",
            vec![ReceiptToken],
            true,
            every(|| Not(ReceiptClaim)),
        ),
        (
            "spawning",
            vec![ReceiptSpawning],
            true,
            every(|| Not(Phase(super::Phase::Spawning))),
        ),
        (
            "expired",
            vec![ReceiptExpired],
            true,
            vec![
                (Policy::Unchecked, Outcome::Pending),
                (Policy::Receipt, Not(Expired)),
                (Policy::Budget { spent: false }, Not(Expired)),
                (Policy::Budget { spent: true }, Not(Expired)),
            ],
        ),
        (
            "claim absent",
            vec![ClaimAbsent],
            true,
            unexpired(|| Not(Claim)),
        ),
        (
            "claim other",
            vec![ClaimOther],
            true,
            unexpired(|| Not(Claim)),
        ),
        (
            "claim unreadable",
            vec![ClaimUnreadable],
            true,
            unexpired(|| ClaimError),
        ),
        // One order: status, tombstone, receipt, deadline, current Claim.
        (
            "status before every other fact",
            vec![
                Tombstone,
                ReceiptSpawning,
                ReceiptExpired,
                ClaimUnreadable,
                StatusDamaged,
            ],
            true,
            every(|| StatusError),
        ),
        (
            "status read after expiry",
            vec![ReceiptExpired, StatusClosed],
            true,
            every(|| Not(State(SessionState::Closed))),
        ),
        (
            "launching status before a damaged tombstone",
            vec![StatusClosed, TombstoneDamaged],
            true,
            every(|| Not(State(SessionState::Closed))),
        ),
        (
            "tombstone before the receipt and the Claim",
            vec![ReceiptSpawning, ReceiptExpired, ClaimUnreadable, Tombstone],
            true,
            every(|| Not(Closed)),
        ),
        (
            "damaged tombstone before the receipt",
            vec![ReceiptSpawning, TombstoneDamaged],
            true,
            every(|| TombstoneError),
        ),
        (
            "receipt Claim before phase",
            vec![ReceiptToken, ReceiptSpawning],
            true,
            every(|| Not(ReceiptClaim)),
        ),
        (
            "phase before deadline",
            vec![ReceiptSpawning, ReceiptExpired],
            true,
            every(|| Not(Phase(super::Phase::Spawning))),
        ),
        (
            "deadline before an unreadable Claim",
            vec![ReceiptExpired, ClaimUnreadable],
            true,
            vec![
                (Policy::Unchecked, ClaimError),
                (Policy::Receipt, Not(Expired)),
                (Policy::Budget { spent: false }, Not(Expired)),
                (Policy::Budget { spent: true }, Not(Expired)),
            ],
        ),
        (
            "deadline before another Claim",
            vec![ReceiptExpired, ClaimOther],
            true,
            vec![
                (Policy::Unchecked, Not(Claim)),
                (Policy::Receipt, Not(Expired)),
                (Policy::Budget { spent: false }, Not(Expired)),
                (Policy::Budget { spent: true }, Not(Expired)),
            ],
        ),
    ];
    #[cfg(unix)]
    cases.extend([
        (
            "tombstone link",
            vec![TombstoneLink],
            true,
            every(|| TombstoneError),
        ),
        (
            "dangling tombstone link",
            vec![TombstoneDanglingLink],
            true,
            every(|| TombstoneError),
        ),
    ]);
    let mut failures = Vec::new();
    for (case, changes, own, outcomes) in cases {
        for (policy, outcome) in outcomes {
            let (_directory, store, token) = directory(&changes);
            let token = if own { token } else { "another".to_owned() };
            let budget_reads = Cell::new(0);
            let expected = expected(&store, &outcome);
            let actual = judged(&store, &token, policy, &budget_reads);
            if actual != expected {
                failures.push(format!("{case} {policy:?}: {actual:?} != {expected:?}"));
            }
            // The budget is read with the deadline, only after the earlier facts held.
            let reached_deadline = matches!(policy, Policy::Budget { .. })
                && (matches!(
                    outcome,
                    Outcome::Pending | Not(Expired | Claim) | ClaimError
                ));
            if budget_reads.get() != usize::from(reached_deadline) {
                failures.push(format!(
                    "{case} {policy:?}: budget read {} times",
                    budget_reads.get()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_launch_without_a_receipt_is_judged_on_status_and_tombstone_alone() {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases = vec![
        ("launching", vec![ReceiptMissing], Outcome::Pending),
        (
            "status closed",
            vec![ReceiptMissing, StatusClosed],
            Outcome::Not(NotPending::State(SessionState::Closed)),
        ),
        (
            "status damaged",
            vec![ReceiptMissing, StatusDamaged],
            Outcome::StatusError,
        ),
        (
            "tombstone",
            vec![ReceiptMissing, Tombstone],
            Outcome::Not(NotPending::Closed),
        ),
        (
            "tombstone damaged",
            vec![ReceiptMissing, TombstoneDamaged],
            Outcome::TombstoneError,
        ),
        // The Claim is not read.
        (
            "claim unreadable",
            vec![ReceiptMissing, ClaimUnreadable],
            Outcome::Pending,
        ),
    ];
    #[cfg(unix)]
    cases.push((
        "tombstone link",
        vec![ReceiptMissing, TombstoneLink],
        Outcome::TombstoneError,
    ));
    for (case, changes, outcome) in cases {
        let (_directory, store, _) = directory(&changes);
        let actual = match launching(&store) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(not_pending)) => Err(format!("{not_pending:?}")),
            Err(error) => Err(format!("{error:#}")),
        };
        assert_eq!(actual, expected(&store, &outcome), "{case}");
    }
}

#[test]
fn the_bound_surface_names_this_session_or_is_absent() {
    let (_directory, store, _) = directory(&[]);
    let id = store
        .directory()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let bound = |value: serde_json::Value| {
        store
            .record(CoreRecord::Terminal)
            .write_json(&value)
            .unwrap();
        bound_surface(&store, &id, || "invalid test surface binding")
            .map(|surface| surface.map(|surface| (surface.kind, surface.id)))
            .map_err(|error| format!("{error:#}"))
    };
    assert_eq!(
        bound_surface(&store, &id, || "invalid test surface binding").unwrap(),
        None
    );
    assert_eq!(
        bound(
            serde_json::json!({"terminal": "wezterm", "session_id": "pane", "managed_session_id": id})
        ),
        Ok(Some((
            crate::native::terminal::TerminalKind::WezTerm,
            "pane".into()
        )))
    );
    // An iTerm2 handle from before the binding names no Session and still counts as bound.
    assert_eq!(
        bound(serde_json::json!({"terminal": "iterm2", "session_id": "legacy"})),
        Ok(Some((
            crate::native::terminal::TerminalKind::Iterm2,
            "legacy".into()
        )))
    );
    assert_eq!(
        bound(serde_json::json!({"terminal": "ghostty", "session_id": "surface"})),
        Err("terminal handle is missing its managed session binding".into())
    );
    assert_eq!(
        bound(
            serde_json::json!({"terminal": "ghostty", "session_id": "surface", "managed_session_id": "session-other"})
        ),
        Err(format!(
            "terminal handle belongs to managed session session-other, not {id}"
        ))
    );
    std::fs::write(store.record(CoreRecord::Terminal).path(), b"{").unwrap();
    let error = bound_surface(&store, &id, || "invalid test surface binding").unwrap_err();
    assert_eq!(error.to_string(), "invalid test surface binding");
    assert!(error.downcast_ref::<serde_json::Error>().is_some());
}
