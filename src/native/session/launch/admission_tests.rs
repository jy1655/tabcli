//! The spawn fence and the wrapper's admission, each case one session directory: which
//! error each reports for every failing fact of the pending-launch judgment, alone and
//! together, and for each damaged record.
use super::test_support::{
    Change::{self, *},
    Expect, apply, pending_launch,
};
use super::*;
use std::{fs, time::Duration};

fn directory(changes: &[Change]) -> (tempfile::TempDir, Store) {
    let (directory, store, _) =
        pending_launch("session-", &std::env::temp_dir(), Duration::from_secs(30));
    apply(&store, changes);
    (directory, store)
}

const CANCELLED_LAUNCHING: &str = "provider launch cancelled: session is launching";
const CANCELLED_CLOSED: &str = "provider launch cancelled: session is closed";
const ATTEMPTED: &str = "provider launch already attempted; refusing a second spawn";
const CLAIM: &str = "provider launch claim no longer belongs to this wrapper";
const EXPIRED: &str = "provider launch deadline expired before spawn";

#[test]
fn spawn_fence_reports_the_first_failing_fact_in_one_order() {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases: Vec<(&str, Vec<Change>, Expect)> = vec![
        ("pending", vec![], Expect::Pass),
        (
            "status closed",
            vec![StatusClosed],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "status failed",
            vec![StatusFailed],
            Expect::Text("provider launch cancelled: session is failed"),
        ),
        ("status damaged", vec![StatusDamaged], Expect::Status),
        (
            "tombstone",
            vec![Tombstone],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        // The tombstone is read as a regular file and decoded.
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "tombstone directory",
            vec![TombstoneDirectory],
            Expect::Tombstone,
        ),
        ("receipt token", vec![ReceiptToken], Expect::Text(CLAIM)),
        ("spawning", vec![ReceiptSpawning], Expect::Text(ATTEMPTED)),
        ("expired", vec![ReceiptExpired], Expect::Text(EXPIRED)),
        ("claim absent", vec![ClaimAbsent], Expect::Text(CLAIM)),
        ("claim other", vec![ClaimOther], Expect::Text(CLAIM)),
        ("claim unreadable", vec![ClaimUnreadable], Expect::Claim),
        ("receipt damaged", vec![ReceiptDamaged], Expect::Receipt),
        (
            "receipt before status",
            vec![ReceiptDamaged, StatusDamaged],
            Expect::Receipt,
        ),
        (
            "status before tombstone",
            vec![Tombstone, StatusDamaged],
            Expect::Status,
        ),
        (
            "status before damaged tombstone",
            vec![StatusClosed, TombstoneDamaged],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "tombstone before phase",
            vec![Tombstone, ReceiptSpawning],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        (
            "phase before deadline",
            vec![ReceiptSpawning, ReceiptExpired],
            Expect::Text(ATTEMPTED),
        ),
        (
            "phase before unreadable claim",
            vec![ReceiptSpawning, ClaimUnreadable],
            Expect::Text(ATTEMPTED),
        ),
        // The deadline precedes the current Claim, which is read last.
        (
            "expired with another claim",
            vec![ReceiptExpired, ClaimOther],
            Expect::Text(EXPIRED),
        ),
        (
            "expired with unreadable claim",
            vec![ReceiptExpired, ClaimUnreadable],
            Expect::Text(EXPIRED),
        ),
        (
            "status before unreadable claim",
            vec![StatusClosed, ClaimUnreadable],
            Expect::Text(CANCELLED_CLOSED),
        ),
        // Without a receipt only the status and the tombstone are judged.
        ("receipt-less", vec![ReceiptMissing], Expect::Pass),
        (
            "receipt-less status closed",
            vec![ReceiptMissing, StatusClosed],
            Expect::Text(CANCELLED_CLOSED),
        ),
        (
            "receipt-less status damaged",
            vec![ReceiptMissing, StatusDamaged],
            Expect::Status,
        ),
        (
            "receipt-less tombstone",
            vec![ReceiptMissing, Tombstone],
            Expect::Text(CANCELLED_LAUNCHING),
        ),
        (
            "receipt-less damaged tombstone",
            vec![ReceiptMissing, TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "receipt-less unreadable claim",
            vec![ReceiptMissing, ClaimUnreadable],
            Expect::Pass,
        ),
    ];
    #[cfg(unix)]
    cases.extend([
        ("tombstone link", vec![TombstoneLink], Expect::Tombstone),
        (
            "tombstone dangling link",
            vec![TombstoneDanglingLink],
            Expect::Tombstone,
        ),
    ]);
    let mut failures = Vec::new();
    for (case, changes, expect) in cases {
        let (_directory, store) = directory(&changes);
        let expected = expect.error(&store);
        // As `spawn` reads: the receipt first, then the fence under the lifecycle lock.
        let actual = read(&store)
            .and_then(|record| check_spawn_locked(&store, record.as_ref()))
            .err()
            .map(|error| format!("{error:#}"));
        if actual != expected {
            failures.push(format!("{case}: {actual:?} != {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const REFUSED: &str =
    "provider launch cancelled or already attempted; the recorded session is unchanged";
const REFUSAL_LOG: &str = "wrapper_exit_code=1; launch cancelled or already attempted";

#[test]
fn wrapper_admission_reports_the_first_failing_fact_and_logs_only_its_refusal() {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cases: Vec<(&str, Vec<Change>, Expect)> = vec![
        ("pending", vec![], Expect::Pass),
        ("status closed", vec![StatusClosed], Expect::Text(REFUSED)),
        ("status failed", vec![StatusFailed], Expect::Text(REFUSED)),
        ("status damaged", vec![StatusDamaged], Expect::Status),
        // Close's window: the tombstone refuses before the status says closed.
        ("tombstone", vec![Tombstone], Expect::Text(REFUSED)),
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            Expect::Tombstone,
        ),
        (
            "tombstone directory",
            vec![TombstoneDirectory],
            Expect::Tombstone,
        ),
        ("receipt token", vec![ReceiptToken], Expect::Text(REFUSED)),
        ("spawning", vec![ReceiptSpawning], Expect::Text(REFUSED)),
        // The wrapper has no deadline policy; the spawn fence judges the deadline.
        ("expired", vec![ReceiptExpired], Expect::Pass),
        ("claim absent", vec![ClaimAbsent], Expect::Text(REFUSED)),
        ("claim other", vec![ClaimOther], Expect::Text(REFUSED)),
        ("claim unreadable", vec![ClaimUnreadable], Expect::Claim),
        ("receipt damaged", vec![ReceiptDamaged], Expect::Receipt),
        (
            "receipt before status",
            vec![ReceiptDamaged, StatusDamaged],
            Expect::Receipt,
        ),
        (
            "status before tombstone",
            vec![Tombstone, StatusDamaged],
            Expect::Status,
        ),
        (
            "status before damaged tombstone",
            vec![StatusClosed, TombstoneDamaged],
            Expect::Text(REFUSED),
        ),
        (
            "phase before unreadable claim",
            vec![ReceiptSpawning, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "status before unreadable claim",
            vec![StatusClosed, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "tombstone before unreadable claim",
            vec![Tombstone, ClaimUnreadable],
            Expect::Text(REFUSED),
        ),
        (
            "expired with spawning",
            vec![ReceiptExpired, ReceiptSpawning],
            Expect::Text(REFUSED),
        ),
        // Without a receipt the wrapper is admitted without reading the status.
        ("receipt-less", vec![ReceiptMissing], Expect::Pass),
        (
            "receipt-less status damaged",
            vec![ReceiptMissing, StatusDamaged],
            Expect::Pass,
        ),
        (
            "receipt-less tombstone",
            vec![ReceiptMissing, Tombstone],
            Expect::Pass,
        ),
        (
            "receipt-less damaged tombstone",
            vec![ReceiptMissing, TombstoneDamaged],
            Expect::Pass,
        ),
    ];
    #[cfg(unix)]
    cases.push(("tombstone link", vec![TombstoneLink], Expect::Tombstone));
    let mut failures = Vec::new();
    for (case, changes, expect) in cases {
        let (_directory, store) = directory(&changes);
        let expected = expect.error(&store);
        let actual = admit_wrapper(&store)
            .err()
            .map(|error| format!("{error:#}"));
        if actual != expected {
            failures.push(format!("{case}: {actual:?} != {expected:?}"));
        }
        let log = fs::read_to_string(store.directory().join(LOG)).unwrap();
        let logged = log.lines().any(|line| line.ends_with(REFUSAL_LOG));
        if logged != matches!(expect, Expect::Text(REFUSED)) {
            failures.push(format!("{case}: refusal logged={logged}: {log:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
