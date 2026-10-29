use super::*;
use crate::native::session::{Reader, Store};
use std::fs;

const CLAIM: &str = "1-2-3";
const NEXT_CLAIM: &str = "4-5-6";

fn record(
    schema: u32,
    claim_token: &str,
    session_id: &str,
    input_ack_schema: Option<u32>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "schema": schema, "claim_token": claim_token, "session_id": session_id,
    });
    if let Some(ack) = input_ack_schema {
        value["input_ack_schema"] = serde_json::json!(ack);
    }
    value
}

// A record as the extension, an older extension or a fault could leave it in the session
// directory. Records are private, so fixtures go through the private writers.
enum Fixture {
    Absent,
    Json(serde_json::Value),
    Bytes(Vec<u8>),
}

fn install(store: &Store, fixture: &Fixture) {
    let record = store.private(STARTUP_READY_FILE);
    match fixture {
        Fixture::Absent => record.remove().unwrap(),
        Fixture::Json(value) => record.write_json(value).unwrap(),
        Fixture::Bytes(bytes) => record.write_private(bytes).unwrap(),
    }
}

// The JSON record padded with trailing whitespace to exactly `len` bytes: still valid JSON,
// so only the read bound decides.
fn padded(value: &serde_json::Value, len: usize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    assert!(bytes.len() <= len);
    bytes.resize(len, b' ');
    bytes
}

#[derive(Debug, PartialEq)]
enum Expected {
    Absent,
    Unreadable,
    Present {
        resolves: bool,
        cancel: bool,
        ack: bool,
    },
}

fn observe(directory: &std::path::Path) -> Expected {
    match read(&Reader::open_unchecked(directory)) {
        StartupReadiness::Absent => Expected::Absent,
        StartupReadiness::Unreadable(_) => Expected::Unreadable,
        StartupReadiness::Present(ready) => Expected::Present {
            resolves: ready.resolves_startup_for(CLAIM),
            cancel: ready.supports_cancel_for(CLAIM),
            ack: ready.acknowledges_windows_input_for(CLAIM),
        },
    }
}

#[test]
fn one_bounded_read_applies_one_floor_and_answers_every_question() {
    let present = |resolves, cancel, ack| Expected::Present {
        resolves,
        cancel,
        ack,
    };
    let full = record(2, CLAIM, "pi-native", Some(1));
    let cases: Vec<(&str, Fixture, Expected)> = vec![
        ("absent", Fixture::Absent, Expected::Absent),
        (
            "malformed_json",
            Fixture::Bytes(b"{".to_vec()),
            Expected::Unreadable,
        ),
        (
            "invalid_utf8",
            Fixture::Bytes(vec![0xff]),
            Expected::Unreadable,
        ),
        (
            "wrong_shape",
            Fixture::Json(serde_json::json!({"schema": "2"})),
            Expected::Unreadable,
        ),
        (
            "schema_0",
            Fixture::Json(record(0, CLAIM, "pi-native", Some(1))),
            Expected::Unreadable,
        ),
        (
            "schema_3",
            Fixture::Json(record(3, CLAIM, "pi-native", Some(1))),
            Expected::Unreadable,
        ),
        (
            "schema_1",
            Fixture::Json(record(1, CLAIM, "pi-native", None)),
            present(true, false, false),
        ),
        (
            "schema_1_claiming_ack",
            Fixture::Json(record(1, CLAIM, "pi-native", Some(1))),
            present(true, false, false),
        ),
        (
            "schema_2_without_ack",
            Fixture::Json(record(2, CLAIM, "pi-native", None)),
            present(true, true, false),
        ),
        (
            "schema_2_ack_1",
            Fixture::Json(full.clone()),
            present(true, true, true),
        ),
        (
            "schema_2_ack_2",
            Fixture::Json(record(2, CLAIM, "pi-native", Some(2))),
            present(true, true, false),
        ),
        (
            "blank_id",
            Fixture::Json(record(2, CLAIM, " ", Some(1))),
            Expected::Unreadable,
        ),
        (
            "id_256_bytes",
            Fixture::Json(record(2, CLAIM, &"s".repeat(256), Some(1))),
            present(true, true, true),
        ),
        (
            "id_257_bytes",
            Fixture::Json(record(2, CLAIM, &"s".repeat(257), Some(1))),
            Expected::Unreadable,
        ),
        (
            "id_with_newline",
            Fixture::Json(record(2, CLAIM, "pi\nnative", Some(1))),
            Expected::Unreadable,
        ),
        (
            "id_with_delete",
            Fixture::Json(record(2, CLAIM, "pi\u{7f}native", Some(1))),
            Expected::Unreadable,
        ),
        (
            "invalid_claim_token",
            Fixture::Json(record(2, "claim", "pi-native", Some(1))),
            Expected::Unreadable,
        ),
        (
            "claim_mismatch",
            Fixture::Json(record(2, NEXT_CLAIM, "pi-native", Some(1))),
            present(false, false, false),
        ),
        (
            "at_the_bound",
            Fixture::Bytes(padded(&full, 4096)),
            present(true, true, true),
        ),
        (
            "over_the_bound",
            Fixture::Bytes(padded(&full, 4097)),
            Expected::Unreadable,
        ),
    ];
    for (name, fixture, expected) in cases {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        install(&store, &fixture);
        // No `turn.claim` exists here: the reader answers from the record and the
        // caller's expected token alone.
        assert_eq!(observe(directory.path()), expected, "{name}");
    }
}

#[test]
fn carry_forward_rewrites_only_a_present_schema_two_record() {
    let full = record(2, CLAIM, "pi-native", Some(1));
    for (name, fixture, rewritten) in [
        ("absent", Fixture::Absent, None),
        ("malformed_json", Fixture::Bytes(b"{".to_vec()), None),
        (
            "schema_1",
            Fixture::Json(record(1, CLAIM, "pi-native", None)),
            None,
        ),
        (
            "schema_1_claiming_ack",
            Fixture::Json(record(1, CLAIM, "pi-native", Some(1))),
            None,
        ),
        (
            "invalid_id",
            Fixture::Json(record(2, CLAIM, "pi\nnative", Some(1))),
            None,
        ),
        ("over_the_bound", Fixture::Bytes(padded(&full, 4097)), None),
        (
            "schema_2_without_ack",
            Fixture::Json(record(2, CLAIM, "pi-native", None)),
            Some(record(2, NEXT_CLAIM, "pi-native", None)),
        ),
        (
            "schema_2_ack_1",
            Fixture::Json(full.clone()),
            Some(record(2, NEXT_CLAIM, "pi-native", Some(1))),
        ),
        (
            "schema_2_already_carried",
            Fixture::Json(record(2, NEXT_CLAIM, "pi-native", Some(1))),
            Some(record(2, NEXT_CLAIM, "pi-native", Some(1))),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        install(&store, &fixture);
        let path = directory.path().join(STARTUP_READY_FILE);
        let before = fs::read(&path).ok();
        carry_forward(&store, NEXT_CLAIM).unwrap();
        let after = fs::read(&path).ok();
        match rewritten {
            None => assert_eq!(after, before, "{name}: left as written"),
            Some(expected) => {
                let written: serde_json::Value =
                    serde_json::from_slice(after.as_deref().unwrap()).unwrap();
                assert_eq!(written, expected, "{name}");
                let StartupReadiness::Present(ready) = read(&store) else {
                    panic!("{name}: carried record must stay readable");
                };
                assert!(ready.supports_cancel_for(NEXT_CLAIM), "{name}");
                assert!(!ready.supports_cancel_for(CLAIM), "{name}");
                assert_eq!(
                    ready.acknowledges_windows_input_for(NEXT_CLAIM),
                    expected["input_ack_schema"] == 1,
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn carry_forward_refuses_an_invalid_next_claim_without_touching_the_record() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    install(
        &store,
        &Fixture::Json(record(2, CLAIM, "pi-native", Some(1))),
    );
    let path = directory.path().join(STARTUP_READY_FILE);
    let before = fs::read(&path).unwrap();
    assert!(carry_forward(&store, "claim").is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn session_id_rule_is_shared_with_the_input_receipt() {
    let long = "s".repeat(256);
    let longer = "s".repeat(257);
    let cases: [(&str, bool); 9] = [
        ("", false),
        ("   ", false),
        ("pi", true),
        ("pi native", true),
        (long.as_str(), true),
        (longer.as_str(), false),
        ("pi\n", false),
        ("pi\tnative", false),
        ("pi\u{7f}", false),
    ];
    for (id, valid) in cases {
        assert_eq!(valid_session_id(id), valid, "{id:?}");
    }
    // The Windows input receipt applies the same rule through the same function.
    use sha2::{Digest, Sha256};
    let prompt = "framed prompt";
    let fingerprint = super::super::input::Fingerprint::of(prompt);
    for (id, valid) in [(long.as_str(), true), (longer.as_str(), false)] {
        let directory = tempfile::tempdir().unwrap();
        Store::open_unchecked(directory.path())
            .private(&format!("pi-input-accepted-{CLAIM}.json"))
            .write_json(&serde_json::json!({
                "schema": 1, "signal": "agent_start", "claim_token": CLAIM,
                "prompt_sha256": format!("{:x}", Sha256::digest(prompt.as_bytes())),
                "prompt_utf8_bytes": prompt.len(), "session_id": id,
            }))
            .unwrap();
        let accepted = super::super::input::read_accepted(directory.path(), CLAIM, &fingerprint);
        assert_eq!(accepted.is_ok(), valid, "{}", id.len());
        if valid {
            assert!(accepted.unwrap());
        }
    }
}
