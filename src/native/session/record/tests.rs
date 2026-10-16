use super::*;
use crate::native::session::primitives::write_json_atomic;
use crate::native::session::turn::JournaledEventState;
use crate::native::session::{SessionState, names::*};
use crate::native::tests::seed_close_fixture;
use std::fs;

const MANIFEST: &str = r#"{"schema":1,"id":"session-a","provider":"codex","provider_path":"/usr/bin/codex","provider_version":"1","workspace":"/w","title":"t","yolo":false,"created_unix_ms":5}"#;
const STATUS: &str =
    r#"{"state":"ready","generation":2,"updated_unix_ms":7,"exit_code":null,"error":null}"#;
const JOURNAL: &str = r#"{"schema":1,"claim_token":"1-2-3","event_file":"event-1.json","event":{"provider":"codex","message":"done","error":null,"provider_session_id":null,"turn_id":null,"created_unix_ms":9},"status_error":null,"status_state":"ready"}"#;
const LAUNCH: &str =
    r#"{"schema":1,"claim_token":"1-2-3","deadline_unix_ms":11,"phase":"pending"}"#;
const RECEIPT: &str = r#"{"schema":1,"request_id":"request-1","claim_token":"1-2-3","event_file":"event-1.json","created_unix_ms":13}"#;
const OVERFLOW: &str = r#"{"nested":[{"created_unix_ms":18446744073709551616}]}"#;

enum Expect {
    Decoded,
    Malformed(&'static str),
    Rule(&'static str),
}
use Expect::{Decoded, Malformed, Rule};

fn check<T>(name: &str, outcome: Result<T, DecodeError>, expected: &Expect) {
    match (outcome, expected) {
        (Ok(_), Decoded) => (),
        (Err(DecodeError::Malformed(error)), Malformed(text))
        | (Err(DecodeError::Rule(error)), Rule(text)) => {
            assert!(format!("{error:#}").contains(text), "{name}: {error:#}");
        }
        (Err(error), _) => panic!("{name}: {error}"),
        (Ok(_), _) => panic!("{name}: decoded"),
    }
}

fn field(record: &str, key: &str, value: serde_json::Value) -> Vec<u8> {
    let mut value_map: serde_json::Value = serde_json::from_str(record).unwrap();
    value_map[key] = value;
    serde_json::to_vec(&value_map).unwrap()
}

// The rows every decoder shares: the bytes, the JSON, the timestamp range.
fn shared_rows(record: &'static str) -> Vec<(&'static str, Vec<u8>, Expect)> {
    vec![
        ("valid", record.as_bytes().to_vec(), Decoded),
        (
            "invalid JSON",
            b"{".to_vec(),
            Malformed("EOF while parsing"),
        ),
        (
            "invalid UTF-8",
            b"\xff".to_vec(),
            Malformed("invalid UTF-8"),
        ),
        (
            "timestamp out of range",
            OVERFLOW.as_bytes().to_vec(),
            Malformed("record timestamp is outside the JSON unsigned integer range"),
        ),
        (
            "nested timestamp out of range",
            field(record, "extra", serde_json::from_str(OVERFLOW).unwrap()),
            Malformed("record timestamp is outside the JSON unsigned integer range"),
        ),
    ]
}

#[test]
fn manifest_decoder_checks_shape_schema_and_id() {
    let mut rows = shared_rows(MANIFEST);
    rows.push((
        "schema mismatch",
        field(MANIFEST, "schema", 999.into()),
        Rule("unsupported session schema 999 for session-a"),
    ));
    rows.push((
        "id mismatch",
        field(MANIFEST, "id", "session-b".into()),
        Rule("session manifest id does not match its directory"),
    ));
    rows.push((
        "missing field",
        b"{\"schema\":1}".to_vec(),
        Malformed("missing field"),
    ));
    for (name, bytes, expected) in &rows {
        check(name, manifest(bytes, "session-a"), expected);
    }
    let decoded = manifest(MANIFEST.as_bytes(), "session-a").unwrap();
    assert_eq!(
        (decoded.id.as_str(), decoded.created_unix_ms),
        ("session-a", 5)
    );
}

#[test]
fn status_decoder_keeps_legacy_records_readable() {
    let mut rows = shared_rows(STATUS);
    rows.push((
        "legacy without generation",
        br#"{"state":"ready","updated_unix_ms":1,"exit_code":null,"error":null}"#.to_vec(),
        Decoded,
    ));
    rows.push((
        "unknown state string",
        field(STATUS, "state", "resume-pending".into()),
        Decoded,
    ));
    rows.push((
        "arbitrary state string",
        field(STATUS, "state", "mystery".into()),
        Decoded,
    ));
    rows.push((
        "state is not a string",
        field(STATUS, "state", 1.into()),
        Malformed("invalid type"),
    ));
    rows.push((
        "diagnostics",
        field(STATUS, "residual_surface", "unverified".into()),
        Decoded,
    ));
    for (name, bytes, expected) in &rows {
        check(name, status(bytes), expected);
    }
    let legacy =
        status(br#"{"state":"ready","updated_unix_ms":1,"exit_code":null,"error":null}"#).unwrap();
    assert_eq!((legacy.generation, legacy.state), (0, SessionState::Ready));
    assert_eq!(
        status(&field(STATUS, "state", "mystery".into()))
            .unwrap()
            .state,
        SessionState::Unknown("mystery".to_owned())
    );
    assert_eq!(
        status(&field(STATUS, "state", "resume-pending".into()))
            .unwrap()
            .state,
        SessionState::ResumePending
    );
}

#[test]
fn journal_decoder_validates_the_completion() {
    let mut rows = shared_rows(JOURNAL);
    rows.push((
        "event file name",
        field(JOURNAL, "event_file", "../escape.json".into()),
        Rule("invalid pending native completion event file"),
    ));
    rows.push((
        "identity",
        field(JOURNAL, "schema", 2.into()),
        Rule("invalid pending native completion identity"),
    ));
    let mut no_time: serde_json::Value = serde_json::from_str(JOURNAL).unwrap();
    no_time["event"]["created_unix_ms"] = serde_json::Value::Null;
    rows.push((
        "missing event time",
        serde_json::to_vec(&no_time).unwrap(),
        Rule("pending native completion event is missing created_unix_ms"),
    ));
    for (name, bytes, expected) in &rows {
        check(name, journal(bytes), expected);
    }
}

#[test]
fn launch_decoder_checks_the_identity_rule() {
    let mut rows = shared_rows(LAUNCH);
    rows.push((
        "schema",
        field(LAUNCH, "schema", 2.into()),
        Rule("invalid launch receipt identity"),
    ));
    rows.push((
        "empty token",
        field(LAUNCH, "claim_token", "".into()),
        Rule("invalid launch receipt identity"),
    ));
    rows.push((
        "unknown phase",
        field(LAUNCH, "phase", "done".into()),
        Malformed("unknown variant"),
    ));
    for (name, bytes, expected) in &rows {
        check(name, launch(bytes), expected);
    }
}

#[test]
fn receipt_decoder_validates_and_binds_the_token() {
    let mut rows = shared_rows(RECEIPT);
    rows.push((
        "token mismatch",
        field(RECEIPT, "claim_token", "4-5-6".into()),
        Rule("Bridge request receipt belongs to a different claim"),
    ));
    rows.push((
        "invalid receipt",
        field(RECEIPT, "request_id", "not-a-request".into()),
        Rule("invalid Bridge request receipt"),
    ));
    rows.push((
        "missing time stays readable",
        field(RECEIPT, "created_unix_ms", serde_json::Value::Null),
        Decoded,
    ));
    for (name, bytes, expected) in &rows {
        check(name, receipt(bytes, "1-2-3"), expected);
    }
}

#[test]
fn decode_error_context_names_only_malformed_bytes() {
    let malformed = DecodeError::Malformed(anyhow::anyhow!("inner"));
    assert_eq!(
        format!("{:#}", malformed.in_path(Path::new("/s/status.json"))),
        "invalid JSON in /s/status.json: inner"
    );
    let rule = DecodeError::Rule(anyhow::anyhow!("the rule"));
    assert_eq!(format!("{:#}", rule.context("ignored")), "the rule");
}

#[test]
fn receipt_index_counts_what_does_not_decode_and_sorts_the_rest() {
    let later = field(RECEIPT, "created_unix_ms", 20.into());
    let later = String::from_utf8(later).unwrap();
    let later_b = later
        .replace("\"1-2-3\"", "\"4-5-6\"")
        .replace("request-1", "request-b");
    let later_a = later
        .replace("\"1-2-3\"", "\"7-8-9\"")
        .replace("request-1", "request-a");
    let tokens: Vec<String> = ["4-5-6", "7-8-9", "1-2-3", "absent", "fault", "other"]
        .map(str::to_owned)
        .to_vec();
    let index = receipt_index(&tokens, |token| match token {
        "4-5-6" => Read::Present(later_b.clone().into_bytes()),
        "7-8-9" => Read::Present(later_a.clone().into_bytes()),
        "1-2-3" => Read::Present(RECEIPT.as_bytes().to_vec()),
        "absent" => Read::Absent,
        "fault" => Read::Fault("denied".to_owned()),
        _ => Read::Present(RECEIPT.as_bytes().to_vec()), // belongs to 1-2-3, not `other`
    });
    assert_eq!(index.unreadable, 3);
    assert_eq!(
        index
            .receipts
            .iter()
            .map(|receipt| receipt.request_id.as_str())
            .collect::<Vec<_>>(),
        ["request-1", "request-a", "request-b"]
    );
}

// B1 and B3 through the Live adapter: the state bytes are decoded strictly, and a
// timestamp outside the range makes the record unreadable; a launch record and a
// manifest fail on their own rules.
#[test]
fn live_snapshot_fails_on_bytes_a_decoder_refuses() {
    for (record, bytes, expected) in [
        (CoreRecord::Status, b"\xff".as_slice(), "invalid UTF-8"),
        (
            CoreRecord::Status,
            OVERFLOW.as_bytes(),
            "record timestamp is outside the JSON unsigned integer range",
        ),
        (CoreRecord::Completion, b"\xff".as_slice(), "invalid UTF-8"),
        (
            CoreRecord::Launch,
            br#"{"schema":2,"claim_token":"x","deadline_unix_ms":1,"phase":"pending"}"#,
            "invalid launch receipt identity",
        ),
        (CoreRecord::Launch, b"{", "invalid launch receipt"),
        (CoreRecord::Manifest, b"\xff", "invalid UTF-8"),
    ] {
        let fixture = seed_close_fixture(JournaledEventState::Committed);
        let reader = Reader::open_unchecked(&fixture.directory);
        fs::write(reader.record(record).path(), bytes).unwrap();
        let error = format!("{:#}", reader.snapshot().err().unwrap());
        assert!(error.contains(expected), "{}: {error}", record.name());
        if record.name() == CoreRecord::Manifest.name() {
            assert!(error.contains("manifest.json"), "{error}");
        }
    }
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(reader.record(CoreRecord::Manifest).path()).unwrap())
            .unwrap();
    manifest["schema"] = 999.into();
    write_json_atomic(reader.record(CoreRecord::Manifest).path(), &manifest).unwrap();
    assert!(
        reader
            .snapshot()
            .err()
            .unwrap()
            .to_string()
            .starts_with("unsupported session schema 999")
    );
    manifest["schema"] = 1.into();
    manifest["id"] = "session-other".into();
    write_json_atomic(reader.record(CoreRecord::Manifest).path(), &manifest).unwrap();
    assert_eq!(
        reader.snapshot().err().unwrap().to_string(),
        "session manifest id does not match its directory"
    );
    fs::remove_file(reader.record(CoreRecord::Manifest).path()).unwrap();
    assert!(
        reader
            .snapshot()
            .err()
            .unwrap()
            .to_string()
            .starts_with("failed to read ")
    );
    fs::remove_file(reader.record(CoreRecord::Status).path()).unwrap();
    assert_eq!(
        reader.snapshot().err().unwrap().to_string(),
        "session has no status record"
    );
}

#[test]
fn live_snapshot_counts_receipts_that_fail_their_rules() {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let requests = reader.record(CoreRecord::Requests).path().to_owned();
    fs::write(requests.join("9-9-9.json"), RECEIPT).unwrap(); // bound to 1-2-3
    fs::write(requests.join("8-8-8.json"), b"\xff").unwrap();
    fs::write(requests.join("not-a-token.json"), RECEIPT).unwrap(); // skipped, not counted
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.unreadable_requests, 2);
    assert_eq!(snapshot.receipts.len(), 1);
    assert!(snapshot.request_index_error.is_none());
    assert_eq!(reader.published_result_count().unwrap(), 1);
}

#[test]
fn required_status_reports_a_missing_record_with_its_cause_and_malformed_bytes_with_its_path() {
    let directory = tempfile::tempdir().unwrap();
    let reader = Reader::open_unchecked(directory.path());
    let path = directory.path().join(STATUS_FILE);
    let error = reader.status().unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("failed to read {}", path.display())
    );
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(reader.status_if_present().unwrap().is_none());
    assert!(reader.closed_if_present().unwrap().is_none());
    fs::write(&path, b"{").unwrap();
    for error in [
        reader.status().unwrap_err(),
        reader.status_if_present().unwrap_err(),
    ] {
        assert_eq!(
            error.to_string(),
            format!("invalid JSON in {}", path.display())
        );
    }
    fs::write(&path, b"\xff").unwrap();
    assert!(format!("{:#}", reader.status().unwrap_err()).contains("invalid UTF-8"));
    fs::write(&path, OVERFLOW).unwrap();
    assert!(
        format!("{:#}", reader.status_if_present().unwrap_err())
            .contains("record timestamp is outside the JSON unsigned integer range")
    );
    fs::write(&path, STATUS).unwrap();
    assert_eq!(reader.status().unwrap().generation, 2);
    fs::write(directory.path().join(CLOSED_STATUS_FILE), STATUS).unwrap();
    assert_eq!(reader.closed_if_present().unwrap().unwrap().generation, 2);
}

// B2: the named reads take a regular file only; a link at the record's path is refused
// in every reader that reaches it, including the Live state read.
#[cfg(unix)]
#[test]
fn linked_core_records_are_refused_by_every_reader() {
    use std::os::unix::fs::symlink;
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let reader = Reader::open_unchecked(&fixture.directory);
    let outside = tempfile::tempdir().unwrap();
    let refused = |error: anyhow::Error, name: &str| {
        let text = format!("{error:#}");
        assert!(
            text.contains("refusing non-regular session file") && text.contains(name),
            "{name}: {text}"
        );
    };
    for name in [
        STATUS_FILE,
        CLOSED_STATUS_FILE,
        MANIFEST_FILE,
        TURN_COMPLETION_FILE,
    ] {
        let path = fixture.directory.join(name);
        let kept = fs::read(&path).ok();
        let _ = fs::remove_file(&path);
        fs::write(
            outside.path().join(name),
            kept.as_deref().unwrap_or(STATUS.as_bytes()),
        )
        .unwrap();
        symlink(outside.path().join(name), &path).unwrap();
        match name {
            STATUS_FILE => {
                refused(reader.status().unwrap_err(), name);
                refused(reader.status_if_present().unwrap_err(), name);
            }
            CLOSED_STATUS_FILE => refused(reader.closed_if_present().unwrap_err(), name),
            MANIFEST_FILE => refused(reader.manifest().unwrap_err(), name),
            _ => (),
        }
        // The Live read pins status, journal and launch and reads the manifest; the
        // closed tombstone is not part of a Snapshot.
        if name != CLOSED_STATUS_FILE {
            refused(reader.snapshot().err().unwrap(), name);
        }
        fs::remove_file(&path).unwrap();
        if let Some(kept) = kept {
            fs::write(&path, kept).unwrap();
        }
    }
    assert!(reader.snapshot().is_ok());
}
