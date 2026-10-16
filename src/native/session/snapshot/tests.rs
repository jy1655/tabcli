use super::*;
use crate::native::session::SnapshotBusy;
use crate::native::session::primitives::{with_publication_read_log, write_json_atomic};
use crate::native::session::turn::JournaledEventState;
use crate::native::tests::seed_close_fixture;
use std::fs;

#[test]
fn snapshot_is_busy_when_a_state_record_changes_during_the_read() {
    for record in [
        CoreRecord::Status,
        CoreRecord::TurnClaim,
        CoreRecord::Completion,
        CoreRecord::Launch,
    ] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        write_json_atomic(
            reader.record(CoreRecord::Launch).path(),
            &launch::Record {
                schema: 1,
                claim_token: "1-2-3".to_owned(),
                deadline_unix_ms: 1,
                phase: launch::Phase::Pending,
            },
        )
        .unwrap();
        let name = record.name();
        let outcome = with_snapshot_hook(
            move |directory| {
                let path = directory.join(name);
                let mut bytes = fs::read(&path).unwrap();
                bytes.push(b' '); // Valid text/JSON; only the before/after check rejects it.
                fs::write(&path, &bytes).unwrap();
            },
            || reader.snapshot(),
        );
        assert!(outcome.err().unwrap().is::<SnapshotBusy>(), "{name}");
        assert!(reader.snapshot().is_ok()); // The rejected attempt released its lock.
    }
}

#[test]
fn published_follows_the_journal_and_the_claim() {
    for state in [
        JournaledEventState::Committed,
        JournaledEventState::Absent,
        JournaledEventState::Mismatched,
        JournaledEventState::Oversized(1),
    ] {
        for claimed in [false, true] {
            let fixture =
                seed_close_fixture(if matches!(state, JournaledEventState::Oversized(_)) {
                    JournaledEventState::Committed
                } else {
                    state
                });
            let reader = Reader::open_unchecked(&fixture.directory);
            if !claimed {
                fs::remove_file(reader.record(CoreRecord::TurnClaim).path()).unwrap();
            }
            let snapshot = reader
                .snapshot_within(if matches!(state, JournaledEventState::Oversized(_)) {
                    0
                } else {
                    EVENT_READ_LIMIT
                })
                .unwrap();
            let name = &snapshot.receipts[0].event_file;
            assert_eq!(
                snapshot.published(name),
                state == JournaledEventState::Committed,
                "{state:?}/{claimed}"
            );
            assert!(
                !reader
                    .snapshot_with(PublicationRead::Deferred)
                    .unwrap()
                    .published(name)
            );
        }
    }
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    fs::remove_file(reader.record(CoreRecord::Completion).path()).unwrap();
    for claim in [None, Some("other-claim"), Some("matching")] {
        let mut snapshot = reader.snapshot().unwrap();
        snapshot.claim = claim.map(|c| {
            if c == "matching" {
                snapshot.receipts[0].claim_token.clone()
            } else {
                c.to_owned()
            }
        });
        assert_eq!(
            snapshot.published(&snapshot.receipts[0].event_file),
            claim != Some("matching")
        );
    }
}

#[test]
fn publication_read_charges_the_caller_and_decides_the_same() {
    for state in [
        JournaledEventState::Committed,
        JournaledEventState::Absent,
        JournaledEventState::Mismatched,
    ] {
        let fixture = seed_close_fixture(state);
        let reader = Reader::open_unchecked(&fixture.directory);
        let (snapshot, reads) =
            with_publication_read_log(|| reader.snapshot_with(PublicationRead::Deferred).unwrap());
        assert!(reads.is_empty());
        let name = &snapshot.receipts[0].event_file;
        let bytes = fs::read(reader.event(name).path()).ok();
        let (read, reads) = with_publication_read_log(|| {
            snapshot
                .publication_read(&reader, name, EVENT_READ_LIMIT)
                .unwrap()
        });
        assert_eq!(read.state, state);
        assert_eq!(
            read.bytes_read,
            bytes.as_ref().map_or(0, |b| b.len() as u64)
        );
        assert_eq!(
            reads.len(),
            usize::from(state != JournaledEventState::Absent)
        );
        assert_eq!(
            read.committed_text,
            if state == JournaledEventState::Committed {
                Some(String::from_utf8(bytes.clone().unwrap()).unwrap())
            } else {
                None
            }
        );
        assert!(!snapshot.published(name)); // A scan read does not mutate deferred evidence.
        if let Some(bytes) = bytes {
            let (read, reads) = with_publication_read_log(|| {
                snapshot
                    .publication_read(&reader, name, bytes.len() as u64 - 1)
                    .unwrap()
            });
            assert_eq!(
                read.state,
                JournaledEventState::Oversized(bytes.len() as u64)
            );
            assert_eq!(read.bytes_read, 0);
            assert!(read.committed_text.is_none());
            assert!(reads.is_empty());
        }
    }
}

#[test]
fn active_receipt_is_the_one_whose_claim_is_installed() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut snapshot = reader.snapshot().unwrap();
    let current = snapshot.receipts[0].clone();
    let mut past = current.clone();
    past.claim_token = "1-2-3".to_owned();
    past.request_id = "request-past".to_owned();
    snapshot.receipts.insert(0, past.clone());
    snapshot.launch = Some(launch::Record {
        schema: 1,
        claim_token: past.claim_token.clone(),
        deadline_unix_ms: 1,
        phase: launch::Phase::Pending,
    });
    assert!(snapshot.published(&current.event_file));
    assert!(snapshot.is_active(&current));
    assert!(!snapshot.is_active(&past));
    assert_eq!(
        snapshot.active_receipt().unwrap().request_id,
        current.request_id
    );
    let mut duplicate = current.clone();
    duplicate.request_id = "request-first".to_owned();
    snapshot.receipts.insert(0, duplicate);
    assert_eq!(
        snapshot.active_receipt().unwrap().request_id,
        "request-first"
    );
    snapshot.claim = None;
    assert!(snapshot.active_receipt().is_none());
    assert!(!snapshot.is_active(&current));
}
