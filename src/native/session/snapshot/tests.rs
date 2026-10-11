use super::*;
use crate::native::session::primitives::{with_publication_read_log, write_json_atomic};
use crate::native::tests::seed_close_fixture;
use serde_json::json;
use std::fs;

// Build the same byte-cache shape as timeline, without borrowing query's implementation.
fn records(reader: &Reader) -> Records {
    let mut records = Records::new();
    for record in [
        CoreRecord::Manifest,
        CoreRecord::Status,
        CoreRecord::TurnClaim,
        CoreRecord::Completion,
        CoreRecord::Launch,
    ] {
        records.insert(
            record.name().to_owned(),
            reader
                .record(record)
                .timeline(EVENT_READ_LIMIT)
                .map_err(|e| format!("{e:#}")),
        );
    }
    records.insert("requests/".to_owned(), Ok(Some(vec![1])));
    for entry in fs::read_dir(reader.record(CoreRecord::Requests).path()).unwrap() {
        let entry = entry.unwrap();
        records.insert(
            format!("requests/{}", entry.file_name().to_str().unwrap()),
            Ok(Some(fs::read(entry.path()).unwrap())),
        );
    }
    for path in reader.events().unwrap() {
        records.insert(
            format!("events/{}", path.file_name().unwrap().to_str().unwrap()),
            Ok(Some(fs::read(path).unwrap())),
        );
    }
    records
}

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
fn from_records_and_snapshot_agree_on_publication_and_requests() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let cache = records(&reader);
    let disk = reader.snapshot().unwrap();
    let cached = Snapshot::from_records(&cache).unwrap();
    assert_eq!(
        serde_json::to_value(&disk.receipts).unwrap(),
        serde_json::to_value(&cached.receipts).unwrap()
    );
    assert_eq!(disk.claim, cached.claim);
    assert_eq!(
        disk.active_receipt().unwrap().request_id,
        cached.active_receipt().unwrap().request_id
    );
    assert_eq!(disk.latest_event_id(), cached.latest_event_id());
    for receipt in &disk.receipts {
        assert!(disk.published(&receipt.event_file));
        assert!(cached.published(&receipt.event_file));
    }
    assert!(disk.paths[0].is_absolute());
    assert_eq!(
        cached.paths[0],
        PathBuf::from(format!("events/{}", disk.latest_event_id().unwrap()))
    );
    assert!(disk._lock.is_some());
    assert!(cached._lock.is_none());
    let disk_read = disk.pending_event.as_ref().unwrap();
    assert!(disk_read.bytes_read > 0);
    assert!(disk_read.committed_text.is_some());
    let cached_read = cached.pending_event.as_ref().unwrap();
    assert_eq!(cached_read.bytes_read, 0);
    assert!(cached_read.committed_text.is_none());
}

#[test]
fn from_records_preserves_cache_failure_policies() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let base = records(&reader);
    let mut cache = base.clone();
    cache.insert(
        CoreRecord::Status.name().to_owned(),
        Err("status denied".to_owned()),
    );
    let status = Snapshot::from_records(&cache).unwrap().status;
    assert_eq!(status.state, SessionState::Unknown("unknown".to_owned()));
    assert_eq!((status.generation, status.updated_unix_ms), (0, 0));
    assert!(
        status.exit_code.is_none() && status.error.is_none() && status.residual_surface.is_none()
    );
    for value in [Ok(None), Ok(Some(b"{".to_vec()))] {
        cache.insert(CoreRecord::Status.name().to_owned(), value);
        assert!(Snapshot::from_records(&cache).is_err());
    }
    for record in [CoreRecord::Completion, CoreRecord::Launch] {
        let mut cache = base.clone();
        cache.insert(record.name().to_owned(), Err("unreadable".to_owned()));
        let snapshot = Snapshot::from_records(&cache).unwrap();
        if record.name() == CoreRecord::Completion.name() {
            assert!(snapshot.pending.is_none());
        } else {
            assert!(snapshot.launch.is_none());
        }
        cache.insert(record.name().to_owned(), Ok(Some(b"{".to_vec())));
        assert!(Snapshot::from_records(&cache).is_err());
    }
    let mut cache = base.clone();
    cache.insert(
        "requests/".to_owned(),
        Err("request index denied".to_owned()),
    );
    cache.insert("requests/1-2-3.json".to_owned(), Ok(Some(b"{".to_vec())));
    let snapshot = Snapshot::from_records(&cache).unwrap();
    assert_eq!(
        snapshot.request_index_error.as_deref(),
        Some("request index denied")
    );
    assert_eq!(snapshot.unreadable_requests, 1);
    assert_eq!(snapshot.receipts.len(), 1);
    for (record, bytes, expected) in [
        (
            CoreRecord::Launch,
            br#"{"schema":2,"claim_token":"x","deadline_unix_ms":1,"phase":"pending"}"#.as_slice(),
            "invalid launch receipt identity",
        ),
        (CoreRecord::Status, b"\xff".as_slice(), "invalid UTF-8"),
        (
            CoreRecord::Status,
            br#"{"nested":[{"created_unix_ms":18446744073709551616}]}"#.as_slice(),
            "record timestamp is outside the JSON unsigned integer range",
        ),
    ] {
        let mut cache = base.clone();
        cache.insert(record.name().to_owned(), Ok(Some(bytes.to_vec())));
        assert!(format!("{:#}", Snapshot::from_records(&cache).err().unwrap()).contains(expected));
    }
    let mut cache = base;
    let mut manifest: Value = parsed(&cache, CoreRecord::Manifest.name())
        .unwrap()
        .unwrap();
    manifest["schema"] = json!(999);
    cache.insert(
        CoreRecord::Manifest.name().to_owned(),
        Ok(Some(serde_json::to_vec(&manifest).unwrap())),
    );
    assert!(
        Snapshot::from_records(&cache)
            .err()
            .unwrap()
            .to_string()
            .starts_with("unsupported session schema 999")
    );
}

#[test]
fn from_records_does_not_fill_filtered_or_unreadable_events_from_disk() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let disk = reader.snapshot().unwrap();
    let name = disk.receipts[0].event_file.clone();
    let source = format!("events/{name}");
    for entry in [None, Some(Err("unreadable".to_owned())), Some(Ok(None))] {
        let mut cache = records(&reader);
        cache.remove(&source);
        if let Some(entry) = entry {
            cache.insert(source.clone(), entry);
        }
        let cached = Snapshot::from_records(&cache).unwrap();
        assert_eq!(
            cached.pending_event.unwrap().state,
            JournaledEventState::Mismatched
        );
        assert!(disk.published(&name));
    }
    drop(disk);
    fs::remove_file(reader.event(&name).path()).unwrap();
    assert_eq!(
        reader.snapshot().unwrap().pending_event.unwrap().state,
        JournaledEventState::Absent
    );
    assert_eq!(
        Snapshot::from_records(&records(&reader))
            .unwrap()
            .pending_event
            .unwrap()
            .state,
        JournaledEventState::Mismatched
    );
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
