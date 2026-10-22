use super::*;
use crate::native::provider::codex::test_support::launch_session;
use crate::native::query::{with_event_read_hook, with_snapshot_retry_window};
use crate::native::reopen::tests::snapshot_directory;
use crate::native::session::{SessionState, Store, update_status};
use agent_bridge::FirstPartyCli;
use std::cell::Cell;
use std::fs::File;
use std::rc::Rc;
use std::time::Duration;

#[test]
fn terminal_reread_releases_snapshot_lock_and_preserves_strict_reader_errors() {
    let root = tempfile::tempdir().unwrap();
    let directory = launch_session(root.path(), Some("0.163.0"));
    let reader = Reader::open_unchecked(&directory);
    let store = Store::open_unchecked(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let receipt = requests::list(&reader).unwrap().receipts.pop().unwrap();
    turn::Report::for_claim(&store, FirstPartyCli::Codex, Some(&receipt.claim_token))
        .with_provenance(Some("codex_notify"))
        .complete("answer", None, None)
        .unwrap();
    let before = snapshot_directory(&directory);
    let lock_path = directory.join("turn.claim.lock");
    let normal_reads = Rc::new(Cell::new(0));
    let seen = normal_reads.clone();
    let observed_lock = lock_path.clone();
    let value = with_event_read_hook(
        move |_| {
            seen.set(seen.get() + 1);
            assert!(matches!(
                File::open(&observed_lock).unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        },
        || {
            terminal_with(&reader, &receipt, |name| {
                assert_eq!(normal_reads.get(), 1);
                assert_eq!(name, receipt.event_file);
                let writer = File::open(&lock_path).unwrap();
                writer.try_lock().unwrap();
                drop(writer);
                reader.event_strict(name).map_err(anyhow::Error::msg)
            })
            .unwrap()
        },
    );
    assert_eq!(value["record"]["request_state"], "completed");
    assert_eq!(value["record"]["provenance"], "codex_notify");
    let error = terminal_with(&reader, &receipt, |_| {
        File::open(&lock_path).unwrap().try_lock().unwrap();
        bail!("strict reread failed")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "strict reread failed");
    assert_eq!(snapshot_directory(&directory), before);
}

#[test]
fn terminal_skips_strict_reread_without_matching_terminal_identity() {
    let root = tempfile::tempdir().unwrap();
    let directory = launch_session(root.path(), Some("0.163.0"));
    let reader = Reader::open_unchecked(&directory);
    let store = Store::open_unchecked(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let receipt = requests::list(&reader).unwrap().receipts.pop().unwrap();
    assert_eq!(
        terminal_with(&reader, &receipt, |_| panic!("pending request")).unwrap(),
        absent()
    );
    turn::Report::for_claim(&store, FirstPartyCli::Codex, Some(&receipt.claim_token))
        .with_provenance(Some("codex_notify"))
        .complete("answer", None, None)
        .unwrap();
    let mut different = receipt.clone();
    different.event_file = "event-other.json".into();
    assert_eq!(
        terminal_with(&reader, &different, |_| panic!("different identity")).unwrap(),
        absent()
    );
    let writer = File::open(directory.join("turn.claim.lock")).unwrap();
    writer.lock().unwrap();
    let error = with_snapshot_retry_window(Duration::ZERO, || {
        terminal_with(&reader, &receipt, |_| panic!("busy snapshot"))
    })
    .unwrap_err();
    assert!(error.is::<crate::native::query::SnapshotBusy>());
}
