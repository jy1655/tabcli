use super::*;
use crate::native::session::{CoreRecord, SessionState, update_status};
use std::fs;

#[cfg(windows)]
fn delivery_without_pi_event(initial: bool) {
    use crate::native::delivery::{DeliveryContext, Turn, deliver_with, test_support::Fixture};
    use std::{cell::Cell, rc::Rc};
    let fixture = Fixture::new();
    let claim = fixture.claim(initial);
    let token = claim.token().to_owned();
    install_pending_turn(fixture.store.directory(), &token).unwrap();
    fixture
        .store
        .private(STARTUP_READY_FILE)
        .write_json(&serde_json::json!({
            "schema": 2, "claim_token": token, "session_id": "pi-input",
            "input_ack_schema": 1,
        }))
        .unwrap();
    let sends = Rc::new(Cell::new(0));
    let count = sends.clone();
    let result = super::test_support::with_console_send(
        Box::new(move |path| {
            count.set(count.get() + 1);
            let frame = fs::read_to_string(path).unwrap();
            assert!(frame.contains("agent-bridge-pi-turn:"));
            assert!(!frame.contains('\n'));
            Ok(())
        }),
        || {
            let mut gate = |_| Ok(());
            deliver_with(
                claim,
                if initial {
                    Turn::Initial { gate: &mut gate }
                } else {
                    Turn::FollowUp { prompt: "prompt" }
                },
                DeliveryContext {
                    deadline: Instant::now() + Duration::from_millis(150),
                    ..fixture.context()
                },
                if initial { &PiInitialPaste } else { &PiPaste },
                &fixture.checks,
            )
        },
    );
    assert_eq!(sends.get(), 1);
    assert!(
        result.is_err(),
        "console batch success alone must remain Uncertain, got {result:?}"
    );
    assert_eq!(
        turn::current_claim_token(&fixture.store)
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    let status = fixture.store.status().unwrap();
    assert_eq!(status.state, SessionState::Working);
    assert!(status.error.unwrap().contains("Pi"));
    assert!(fixture.store.private(PENDING_TURN_FILE).path().exists());
    assert_eq!(
        fixture
            .store
            .record(CoreRecord::InitialPrompt)
            .path()
            .exists(),
        initial
    );
}

#[test]
#[cfg(windows)]
fn input_ack_initial_console_success_without_pi_event_is_uncertain() {
    delivery_without_pi_event(true);
}

#[test]
#[cfg(windows)]
fn input_ack_followup_console_success_without_pi_event_is_uncertain() {
    delivery_without_pi_event(false);
}

#[test]
fn input_ack_capability_survives_two_followups_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    store
        .private(STARTUP_READY_FILE)
        .write_json(&serde_json::json!({
            "schema": 2, "claim_token": pending.claim_token, "session_id": "pi-input",
            "input_ack_schema": 1,
        }))
        .unwrap();
    for _ in 0..2 {
        store.record(CoreRecord::TurnClaim).remove().unwrap();
        let next = turn::claim(&store, &[]).unwrap();
        let token = next.token().to_owned();
        next.retain();
        install_pending_turn(directory.path(), &token).unwrap();
        cancel_pending_turn(directory.path(), &token).unwrap();
        let ready: serde_json::Value = store.private(STARTUP_READY_FILE).json().unwrap();
        assert_eq!(
            ready["input_ack_schema"], 1,
            "carry-forward must retain the extension's declared capability"
        );
        assert_eq!(ready["claim_token"], token);
        assert_eq!(ready["session_id"], "pi-input");
    }
}

#[test]
fn input_ack_custom_event_clears_old_result_and_usage_correlation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("extension.js"), bridge_extension()).unwrap();
    fs::write(root.join("test.mjs"), include_str!("../pi_custom_test.mjs")).unwrap();
    let mut inputs = Vec::new();
    for _ in 0..2 {
        let store = Store::open_unchecked(root);
        store.record(CoreRecord::TurnClaim).remove().unwrap();
        update_status(root, SessionState::Working, None, None).unwrap();
        let pending = super::test_support::claim_pending_turn(root);
        inputs.push(serde_json::json!({
            "claim": fs::read_to_string(store.record(CoreRecord::TurnClaim).path()).unwrap(),
            "token": pending.claim_token,
            "prompt": correlated_prompt("synthetic input", &pending),
        }));
    }
    Store::open_unchecked(root)
        .private("inputs.json")
        .write_json(&inputs)
        .unwrap();
    let output = std::process::Command::new("node")
        .arg(root.join("test.mjs"))
        .arg(root)
        .output()
        .expect("Node on PATH is required to execute the Pi extension tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn input_ack_shipping_extension_round_trips_real_core_and_pending_records() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("extension.mjs"), bridge_extension()).unwrap();
    fs::write(root.join("test.mjs"), include_str!("../pi_input_test.mjs")).unwrap();
    let cases = [
        "shipping",
        "exact",
        "legacy",
        "repeat",
        "retry",
        "manual",
        "marker",
        "altered",
        "other_claim",
        "images",
        "invalid_images",
        "custom",
        "missing",
        "unknown",
        "replaced",
        "retired",
        "retired_after_read",
        "publication_failure",
        "write_failure",
        "post_rename_failure",
        "conversation",
        "bad_binding",
        "large_pending",
        "undelivered",
        "macos",
    ];
    let mut inputs = serde_json::Map::new();
    for name in cases {
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("events")).unwrap();
        let store = Store::open_unchecked(&path);
        update_status(&path, SessionState::Working, None, None).unwrap();
        let surface = serde_json::from_value(
            serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
        )
        .unwrap();
        let mut records = Vec::new();
        for _ in 0..2 {
            store.record(CoreRecord::TurnClaim).remove().unwrap();
            let pending = super::test_support::claim_pending_turn(&path);
            let mut turn =
                super::super::transport_test_context(&path, &surface, delivery::TurnKind::Initial);
            turn.claim_token = &pending.claim_token;
            turn.prompt = "synthetic 한글 😀\t\n\\t\\n\"request\"";
            let prompt = input::prepare_windows(&turn, true).unwrap();
            records.push(serde_json::json!({
                "claim": fs::read_to_string(store.record(CoreRecord::TurnClaim).path()).unwrap(),
                "token": pending.claim_token, "marker": pending.marker, "prompt": prompt,
                "pending": fs::read_to_string(store.private(PENDING_TURN_FILE).path()).unwrap(),
            }));
        }
        inputs.insert(name.to_owned(), serde_json::json!(records));
    }
    Store::open_unchecked(root)
        .private("inputs.json")
        .write_json(&inputs)
        .unwrap();
    let output = std::process::Command::new("node")
        .arg(root.join("test.mjs"))
        .arg(root)
        .output()
        .expect("Node on PATH is required to execute the Pi extension tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outputs: Vec<serde_json::Value> = Store::open_unchecked(root)
        .private("outputs.json")
        .json()
        .unwrap();
    for case in outputs {
        let name = case["name"].as_str().unwrap();
        let path = root.join(name);
        let records = inputs[name].as_array().unwrap();
        for (record, accepted) in records.iter().zip(case["receipts"].as_array().unwrap()) {
            let token = record["token"].as_str().unwrap();
            let fingerprint = input::Fingerprint::of(record["prompt"].as_str().unwrap());
            assert_eq!(
                input::read_accepted(&path, token, &fingerprint).unwrap(),
                accepted.as_bool().unwrap(),
                "{name}"
            );
        }
        let ready: StartupReady = Store::open_unchecked(&path)
            .private(STARTUP_READY_FILE)
            .json()
            .unwrap();
        let expected = if name == "macos" || (name == "shipping" && !cfg!(windows)) {
            None
        } else {
            Some(1)
        };
        assert_eq!(ready.input_ack_schema, expected);
    }
}

#[cfg(windows)]
fn deliver_pi(
    fixture: &delivery::test_support::Fixture,
    claim: turn::Claim,
    initial: bool,
) -> Result<delivery::Delivered> {
    let mut gate = |_| Ok(());
    delivery::deliver_with(
        claim,
        if initial {
            delivery::Turn::Initial { gate: &mut gate }
        } else {
            delivery::Turn::FollowUp {
                prompt: "synthetic request",
            }
        },
        fixture.context(),
        if initial { &PiInitialPaste } else { &PiPaste },
        &fixture.checks,
    )
}

#[cfg(windows)]
fn write_ready(store: &Store, token: &str, schema: u32, capability: Option<u32>) {
    store
        .private(STARTUP_READY_FILE)
        .write_json(&StartupReady {
            schema,
            claim_token: token.into(),
            session_id: "pi-input".into(),
            input_ack_schema: capability,
        })
        .unwrap();
}

#[cfg(windows)]
fn receipt_value(token: &str, prompt: &str) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    serde_json::json!({ "schema":1, "signal":"agent_start", "claim_token":token,
        "prompt_sha256": format!("{:x}", Sha256::digest(prompt.as_bytes())),
        "prompt_utf8_bytes":prompt.len(), "session_id":"pi-input" })
}

#[cfg(windows)]
fn publish_receipt(directory: &Path, token: &str, prompt: &str) {
    Store::open_unchecked(directory)
        .private(&format!("pi-input-accepted-{token}.json"))
        .write_json(&receipt_value(token, prompt))
        .unwrap();
}

#[test]
#[cfg(windows)]
fn input_ack_legacy_or_unknown_capability_refuses_both_transports_before_input() {
    use std::{cell::Cell, rc::Rc};
    for initial in [true, false] {
        for (schema, capability) in [(1, None), (2, None), (2, Some(2))] {
            let fixture = delivery::test_support::Fixture::new();
            let claim = fixture.claim(initial);
            let token = claim.token().to_owned();
            install_pending_turn(fixture.store.directory(), &token).unwrap();
            write_ready(&fixture.store, &token, schema, capability);
            let calls = Rc::new(Cell::new(0));
            let count = calls.clone();
            let error = super::test_support::with_console_send(
                Box::new(move |_| {
                    count.set(count.get() + 1);
                    Ok(())
                }),
                || deliver_pi(&fixture, claim, initial),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("start a new Bridge session"));
            assert_eq!(calls.get(), 0);
            assert!(turn::current_claim_token(&fixture.store).unwrap().is_none());
        }
    }
}

#[test]
#[cfg(windows)]
fn input_ack_proven_prewrite_refusal_rolls_back_without_waiting() {
    for initial in [true, false] {
        let fixture = delivery::test_support::Fixture::new();
        let claim = fixture.claim(initial);
        let token = claim.token().to_owned();
        install_pending_turn(fixture.store.directory(), &token).unwrap();
        write_ready(&fixture.store, &token, 2, Some(1));
        let error = super::test_support::with_console_send(
            Box::new(|_| {
                Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                    "prewrite refusal"
                )))
            }),
            || deliver_pi(&fixture, claim, initial),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("prewrite refusal"));
        assert!(turn::current_claim_token(&fixture.store).unwrap().is_none());
        assert_eq!(
            fixture.store.private(PENDING_TURN_FILE).path().exists(),
            initial
        );
    }
}

#[test]
#[cfg(windows)]
fn input_ack_fast_completion_and_next_claim_preserve_the_first_senders_receipt() {
    for initial in [true, false] {
        let fixture = delivery::test_support::Fixture::new();
        let claim = fixture.claim(initial);
        let token = claim.token().to_owned();
        install_pending_turn(fixture.store.directory(), &token).unwrap();
        write_ready(&fixture.store, &token, 2, Some(1));
        let directory = fixture.store.directory().to_owned();
        let first = token.clone();
        super::test_support::with_console_send(
            Box::new(move |path| {
                let prompt = fs::read_to_string(path).unwrap();
                publish_receipt(&directory, &first, &prompt);
                let store = Store::open_unchecked(&directory);
                turn::Report::for_claim(&store, FirstPartyCli::Pi, Some(&first))
                    .complete("done", None, None)
                    .unwrap();
                let next = turn::claim_ready(&store, "session-test", &[]).unwrap().0;
                let surface = serde_json::from_value(
                    serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
                )
                .unwrap();
                let mut turn = super::super::transport_test_context(
                    &directory,
                    &surface,
                    delivery::TurnKind::FollowUp,
                );
                turn.claim_token = next.token();
                input::prepare_windows(&turn, false).unwrap();
                next.retain();
                Ok(())
            }),
            || deliver_pi(&fixture, claim, initial),
        )
        .unwrap();
        let next = turn::current_claim_token(&fixture.store).unwrap().unwrap();
        assert_ne!(next, token);
        assert_eq!(
            read_pending_turn(fixture.store.directory())
                .unwrap()
                .unwrap()
                .claim_token,
            next
        );
        assert!(fixture.store.status().unwrap().error.is_none());
        assert_eq!(fixture.store.status().unwrap().state, SessionState::Claimed);
        assert_eq!(
            crate::native::session::primitives::event_paths(fixture.store.directory())
                .unwrap()
                .len(),
            1
        );
        cancel_pending_turn(fixture.store.directory(), &token).unwrap();
        assert_eq!(
            read_pending_turn(fixture.store.directory())
                .unwrap()
                .unwrap()
                .claim_token,
            next
        );
        assert!(install_pending_turn(fixture.store.directory(), &token).is_err());
        assert!(
            fixture
                .store
                .private(&format!("pi-input-accepted-{token}.json"))
                .path()
                .exists()
        );
    }
}

#[test]
#[cfg(windows)]
fn input_ack_close_rechecks_the_receipt_once_before_reporting_uncertainty() {
    use std::{cell::Cell, rc::Rc};
    for late_receipt in [false, true] {
        let fixture = delivery::test_support::Fixture::new();
        let claim = fixture.claim(false);
        let token = claim.token().to_owned();
        write_ready(&fixture.store, &token, 2, Some(1));
        let directory = fixture.store.directory().to_owned();
        let closed_directory = directory.clone();
        let receipt_directory = directory.clone();
        let receipt_token = token.clone();
        let reads = Rc::new(Cell::new(0));
        let count = reads.clone();
        let result = super::test_support::with_receipt_hooks(
            Some(Box::new(move || count.set(count.get() + 1))),
            Some(Box::new(move || {
                if late_receipt {
                    let pending = read_pending_turn(&receipt_directory).unwrap().unwrap();
                    let prompt =
                        terminal_correlated_prompt("synthetic request", &pending, true).unwrap();
                    publish_receipt(&receipt_directory, &receipt_token, &prompt);
                }
            })),
            || {
                super::test_support::with_console_send(
                    Box::new(move |_| {
                        update_status(&closed_directory, SessionState::Closed, None, None).unwrap();
                        Ok(())
                    }),
                    || deliver_pi(&fixture, claim, false),
                )
            },
        );
        assert_eq!(result.is_ok(), late_receipt);
        assert_eq!(
            reads.get(),
            2,
            "read once before observing close and once after"
        );
        assert_eq!(fixture.store.status().unwrap().state, SessionState::Closed);
    }
}

#[test]
#[cfg(windows)]
fn input_ack_invalid_receipts_are_uncertain_and_never_roll_back_dispatched_input() {
    for kind in [
        "wrong_digest",
        "wrong_claim",
        "wrong_length",
        "wrong_signal",
        "wrong_schema",
        "unknown_field",
        "bad_id",
        "bad_id_type",
        "bad_length_type",
        "json",
        "utf8",
        "oversized",
        "directory",
        "uncertain_write",
    ] {
        let fixture = delivery::test_support::Fixture::new();
        let claim = fixture.claim(false);
        let token = claim.token().to_owned();
        write_ready(&fixture.store, &token, 2, Some(1));
        let directory = fixture.store.directory().to_owned();
        let receipt_token = token.clone();
        let outcome = super::test_support::with_console_send(
            Box::new(move |path| {
                let prompt = fs::read_to_string(path).unwrap();
                let store = Store::open_unchecked(&directory);
                let record = store.private(&format!("pi-input-accepted-{receipt_token}.json"));
                let mut value = receipt_value(&receipt_token, &prompt);
                match kind {
                    "wrong_digest" => value["prompt_sha256"] = serde_json::json!("0".repeat(64)),
                    "wrong_claim" => value["claim_token"] = serde_json::json!("1-2-3"),
                    "wrong_length" => value["prompt_utf8_bytes"] = serde_json::json!(1),
                    "wrong_signal" => value["signal"] = serde_json::json!("before_agent_start"),
                    "wrong_schema" => value["schema"] = serde_json::json!(2),
                    "unknown_field" => value["extra"] = serde_json::json!(true),
                    "bad_id" => value["session_id"] = serde_json::json!("pi\n"),
                    "bad_id_type" => value["session_id"] = serde_json::json!(7),
                    "bad_length_type" => value["prompt_utf8_bytes"] = serde_json::json!("100"),
                    "json" => {
                        record.write_private(b"{").unwrap();
                        return Ok(());
                    }
                    "utf8" => {
                        record.write_private(&[0xff]).unwrap();
                        return Ok(());
                    }
                    "oversized" => {
                        record.write_private(&[b' '; 4097]).unwrap();
                        return Ok(());
                    }
                    "directory" => {
                        fs::create_dir(record.path()).unwrap();
                        return Ok(());
                    }
                    _ => {}
                }
                record.write_json(&value).unwrap();
                if kind == "uncertain_write" {
                    Err(terminal::TerminalSendFailure::delivery_uncertain(
                        anyhow::anyhow!("write outcome unknown"),
                    ))
                } else {
                    Ok(())
                }
            }),
            || deliver_pi(&fixture, claim, false),
        );
        assert_eq!(outcome.is_ok(), kind == "uncertain_write", "{kind}");
        assert_eq!(
            turn::current_claim_token(&fixture.store)
                .unwrap()
                .as_deref(),
            Some(token.as_str())
        );
        assert!(
            read_pending_turn(fixture.store.directory())
                .unwrap()
                .is_some()
        );
        assert_eq!(fixture.store.status().unwrap().state, SessionState::Working);
        assert!(
            crate::native::session::primitives::event_paths(fixture.store.directory())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
#[cfg(windows)]
fn input_ack_receipt_observed_after_the_original_deadline_cannot_report_sent() {
    let fixture = delivery::test_support::Fixture::new();
    let claim = fixture.claim(false);
    let token = claim.token().to_owned();
    write_ready(&fixture.store, &token, 2, Some(1));
    let directory = fixture.store.directory().to_owned();
    let deadline = Instant::now() + Duration::from_millis(100);
    let result = super::test_support::with_console_send(
        Box::new(move |path| {
            publish_receipt(&directory, &token, &fs::read_to_string(path).unwrap());
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Ok(())
        }),
        || {
            delivery::deliver_with(
                claim,
                delivery::Turn::FollowUp {
                    prompt: "synthetic request",
                },
                delivery::DeliveryContext {
                    deadline,
                    ..fixture.context()
                },
                &PiPaste,
                &fixture.checks,
            )
        },
    );
    assert!(result.is_err());
    assert!(turn::current_claim_token(&fixture.store).unwrap().is_some());
    assert!(
        fixture
            .store
            .status()
            .unwrap()
            .error
            .unwrap()
            .contains("deadline")
    );
}
