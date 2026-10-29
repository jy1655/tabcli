use super::startup_ready::{STARTUP_READY_FILE, StartupReady};
#[cfg(windows)]
use super::test_support::InputScenario;
use super::*;
use crate::native::session::{CoreRecord, SessionState, update_status};
use std::fs;

#[test]
fn pending_turn_without_windows_input_preserves_the_legacy_schema() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    let expected = serde_json::json!({
        "schema": 1,
        "claim_token": pending.claim_token,
        "marker": format!("<!-- agent-bridge-pi-turn:{} -->", pending.claim_token),
    });
    let written: serde_json::Value = store.private(PENDING_TURN_FILE).json().unwrap();
    assert_eq!(written, expected);
    assert!(pending.windows_input.is_none());

    store
        .private(PENDING_TURN_FILE)
        .write_json(&expected)
        .unwrap();
    let read = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(read.schema, 1);
    assert_eq!(read.claim_token, pending.claim_token);
    assert_eq!(read.marker, pending.marker);
    assert!(read.windows_input.is_none());
}

#[test]
fn pending_turn_windows_fingerprint_round_trips_without_changing_core_or_legacy_fields() {
    #[derive(serde::Deserialize)]
    struct LegacyPendingPiTurn {
        schema: u32,
        claim_token: String,
        marker: String,
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    super::test_support::stored_state(directory.path(), &SessionState::Working);
    let claim_before = fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap();
    let status_before = fs::read(store.record(CoreRecord::Status).path()).unwrap();
    let surface = serde_json::from_value(
        serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
    )
    .unwrap();
    let mut context = super::super::transport_test_context(
        directory.path(),
        &surface,
        delivery::TurnKind::Initial,
    );
    context.claim_token = &pending.claim_token;
    context.prompt = "synthetic 한글 😀\t\nrequest";
    let framed = input::prepare_windows(&context, true).unwrap();
    let fingerprint = input::Fingerprint::of(&framed);
    let written: serde_json::Value = store.private(PENDING_TURN_FILE).json().unwrap();
    assert_eq!(
        written,
        serde_json::json!({
            "schema": 1,
            "claim_token": pending.claim_token,
            "marker": pending.marker,
            "windows_input": fingerprint,
        })
    );
    assert!(
        fs::metadata(store.private(PENDING_TURN_FILE).path())
            .unwrap()
            .len()
            <= 4096
    );
    let read = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(read.windows_input.as_ref(), Some(&fingerprint));
    let legacy: LegacyPendingPiTurn = store.private(PENDING_TURN_FILE).json().unwrap();
    assert_eq!(legacy.schema, 1);
    assert_eq!(legacy.claim_token, pending.claim_token);
    assert_eq!(legacy.marker, pending.marker);
    assert_eq!(
        fs::read(store.record(CoreRecord::TurnClaim).path()).unwrap(),
        claim_before
    );
    assert_eq!(
        fs::read(store.record(CoreRecord::Status).path()).unwrap(),
        status_before
    );
}

#[test]
fn pending_turn_reader_rejects_invalid_identity_in_legacy_and_extended_records() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    for fingerprint in [
        None,
        Some(input::Fingerprint::of("synthetic framed prompt")),
    ] {
        install_pending_turn_bound(directory.path(), &pending.claim_token, fingerprint).unwrap();
        let valid: serde_json::Value = store.private(PENDING_TURN_FILE).json().unwrap();
        assert!(read_pending_turn(directory.path()).unwrap().is_some());
        for (field, replacement) in [
            ("schema", serde_json::json!(0)),
            ("schema", serde_json::json!(2)),
            ("claim_token", serde_json::json!("../invalid")),
            ("claim_token", serde_json::json!("1".repeat(161))),
            ("claim_token", serde_json::json!("1-2-3")),
            ("marker", serde_json::json!("unrelated marker")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = replacement;
            store
                .private(PENDING_TURN_FILE)
                .write_json(&invalid)
                .unwrap();
            assert!(read_pending_turn(directory.path()).is_err(), "{invalid}");
        }
    }
}

#[test]
fn pending_turn_reader_bounds_and_validates_the_optional_windows_fingerprint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let pending = super::test_support::claim_pending_turn(directory.path());
    install_pending_turn_bound(
        directory.path(),
        &pending.claim_token,
        Some(input::Fingerprint::of("synthetic framed prompt")),
    )
    .unwrap();
    let valid: serde_json::Value = store.private(PENDING_TURN_FILE).json().unwrap();
    for (field, replacement) in [
        ("sha256", serde_json::json!("a".repeat(63))),
        ("sha256", serde_json::json!("A".repeat(64))),
        ("utf8_bytes", serde_json::json!(0)),
        ("utf8_bytes", serde_json::json!(9_007_199_254_740_992_u64)),
        ("utf8_bytes", serde_json::json!("1")),
        ("extra", serde_json::json!(true)),
    ] {
        let mut invalid = valid.clone();
        invalid["windows_input"][field] = replacement;
        store
            .private(PENDING_TURN_FILE)
            .write_json(&invalid)
            .unwrap();
        assert!(read_pending_turn(directory.path()).is_err(), "{invalid}");
    }
    let mut oversized = serde_json::to_vec(&valid).unwrap();
    oversized.resize(4097, b' ');
    store.private(PENDING_TURN_FILE).remove().unwrap();
    store
        .private(PENDING_TURN_FILE)
        .write_private(&oversized)
        .unwrap();
    assert!(read_pending_turn(directory.path()).is_err());
}

#[cfg(windows)]
fn delivery_without_pi_event(initial: bool) {
    use crate::native::delivery::test_support::Fixture;
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
    let expected = terminal_correlated_prompt(
        if initial {
            "prompt"
        } else {
            "synthetic request"
        },
        &read_pending_turn(fixture.store.directory())
            .unwrap()
            .unwrap(),
        true,
    )
    .unwrap();
    let mut scenario = InputScenario::new(move |path| {
        let frame = fs::read_to_string(path).unwrap();
        assert_eq!(frame, expected);
        assert!(frame.contains("agent-bridge-pi-turn:"));
        assert!(!frame.contains('\n'));
        Ok(())
    });
    scenario
        .clock
        .shared()
        .set(scenario.deadline - Duration::from_millis(100));
    let result = deliver_pi(&fixture, claim, initial, &mut scenario);
    assert_eq!(
        scenario.sends, 1,
        "the missing-receipt path requires console dispatch"
    );
    assert_eq!(scenario.clock.slept, Duration::from_millis(100));
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
    let error = status.error.unwrap();
    assert!(
        error.contains("Pi input submission is uncertain"),
        "{error}"
    );
    assert!(error.contains("deadline"), "{error}");
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
    scenario: &mut InputScenario,
) -> Result<delivery::Delivered> {
    let mut gate = |_| Ok(());
    let deadline = scenario.deadline;
    let transport: Box<dyn Transport + '_> = if initial {
        Box::new(PiInitialPaste {
            input: RefCell::new(scenario),
        })
    } else {
        Box::new(PiPaste {
            input: RefCell::new(scenario),
        })
    };
    delivery::deliver_with(
        claim,
        if initial {
            delivery::Turn::Initial { gate: &mut gate }
        } else {
            delivery::Turn::FollowUp {
                prompt: "synthetic request",
            }
        },
        delivery::DeliveryContext {
            deadline,
            ..fixture.context()
        },
        transport.as_ref(),
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
fn initial_transport_rechecks_startup_before_input_receipt_delivery() {
    let fixture = delivery::test_support::Fixture::new();
    let claim = fixture.claim(true);
    let token = claim.token().to_owned();
    install_pending_turn(fixture.store.directory(), &token).unwrap();
    write_ready(&fixture.store, &token, 2, Some(1));
    let directory = fixture.store.directory().to_owned();
    // Preparation succeeds, then its startup correlation disappears before the
    // actual PiInitialPaste::send. The input capability alone must not skip the
    // initial readiness gate, even though the core Claim and startup receipt match.
    *fixture.checks.before_begin.borrow_mut() = Some(Box::new(move || {
        Store::open_unchecked(&directory)
            .private(PENDING_TURN_FILE)
            .remove()
            .unwrap();
    }));
    let mut scenario = InputScenario::new(|_| panic!("initial readiness was bypassed"));
    let error = deliver_pi(&fixture, claim, true, &mut scenario).unwrap_err();
    assert!(format!("{error:#}").contains("Pi initial turn is missing"));
    assert_eq!(scenario.sends, 0);
    assert_eq!(scenario.reads, 0);
    assert!(turn::current_claim_token(&fixture.store).unwrap().is_none());
}

#[test]
#[cfg(windows)]
fn input_ack_legacy_or_unknown_capability_refuses_both_transports_before_input() {
    for initial in [true, false] {
        for (schema, capability) in [(1, None), (2, None), (2, Some(2))] {
            let fixture = delivery::test_support::Fixture::new();
            let claim = fixture.claim(initial);
            let token = claim.token().to_owned();
            install_pending_turn(fixture.store.directory(), &token).unwrap();
            write_ready(&fixture.store, &token, schema, capability);
            let mut scenario = InputScenario::new(|_| Ok(()));
            let error = deliver_pi(&fixture, claim, initial, &mut scenario).unwrap_err();
            assert!(format!("{error:#}").contains("start a new Bridge session"));
            assert_eq!(scenario.sends, 0);
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
        let mut scenario = InputScenario::new(|_| {
            Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "prewrite refusal"
            )))
        });
        let error = deliver_pi(&fixture, claim, initial, &mut scenario).unwrap_err();
        assert!(format!("{error:#}").contains("prewrite refusal"));
        assert_eq!(scenario.sends, 1);
        assert_eq!(scenario.reads, 0);
        assert_eq!(scenario.clock.slept, Duration::ZERO);
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
        let mut scenario = InputScenario::new(move |path| {
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
        });
        deliver_pi(&fixture, claim, initial, &mut scenario).unwrap();
        assert_eq!(scenario.sends, 1);
        assert_eq!(scenario.reads, 1);
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
    for late_receipt in [false, true] {
        let fixture = delivery::test_support::Fixture::new();
        let claim = fixture.claim(false);
        let token = claim.token().to_owned();
        write_ready(&fixture.store, &token, 2, Some(1));
        let directory = fixture.store.directory().to_owned();
        let closed_directory = directory.clone();
        let receipt_directory = directory.clone();
        let receipt_token = token.clone();
        let mut scenario = InputScenario::new(move |_| {
            update_status(&closed_directory, SessionState::Closed, None, None).unwrap();
            Ok(())
        });
        scenario.current = Some(Box::new(move |turn| {
            let observation = input::current(turn);
            if observation.is_err() && late_receipt {
                let pending = read_pending_turn(&receipt_directory).unwrap().unwrap();
                let prompt =
                    terminal_correlated_prompt("synthetic request", &pending, true).unwrap();
                publish_receipt(&receipt_directory, &receipt_token, &prompt);
            }
            observation
        }));
        let result = deliver_pi(&fixture, claim, false, &mut scenario);
        assert_eq!(result.is_ok(), late_receipt);
        assert_eq!(
            scenario.reads, 2,
            "read once before observing close and once after"
        );
        assert_eq!(scenario.sends, 1);
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
        let mut scenario = InputScenario::new(move |path| {
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
        });
        let outcome = deliver_pi(&fixture, claim, false, &mut scenario);
        assert_eq!(outcome.is_ok(), kind == "uncertain_write", "{kind}");
        assert_eq!(scenario.sends, 1);
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
    let expected = terminal_correlated_prompt(
        "synthetic request",
        &PendingPiTurn::new(&token).unwrap(),
        true,
    )
    .unwrap();
    let receipt_token = token.clone();
    let mut scenario = InputScenario::new(move |path| {
        let frame = fs::read_to_string(path).unwrap();
        assert_eq!(frame, expected);
        publish_receipt(&directory, &receipt_token, &frame);
        Ok(())
    });
    let now = scenario.clock.shared();
    let deadline = scenario.deadline;
    scenario.receipt = Some(Box::new(move |turn, fingerprint| {
        let receipt = input::read_accepted(turn.directory, turn.claim_token, fingerprint);
        now.set(deadline + Duration::from_millis(1));
        receipt
    }));
    let result = deliver_pi(&fixture, claim, false, &mut scenario);
    assert_eq!(
        scenario.sends, 1,
        "the late-receipt path requires console dispatch"
    );
    assert_eq!(
        scenario.reads, 1,
        "the matching receipt crosses the deadline while read"
    );
    assert!(
        result.is_err(),
        "a receipt observed after the original deadline cannot be Sent"
    );
    assert_eq!(
        turn::current_claim_token(&fixture.store)
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    let pending = read_pending_turn(fixture.store.directory())
        .unwrap()
        .unwrap();
    assert_eq!(pending.claim_token, token);
    assert!(
        input::read_accepted(
            fixture.store.directory(),
            &token,
            pending.windows_input.as_ref().unwrap(),
        )
        .unwrap()
    );
    let status = fixture.store.status().unwrap();
    assert_eq!(status.state, SessionState::Working);
    let error = status.error.unwrap();
    assert!(
        error.contains("Pi input submission is uncertain"),
        "{error}"
    );
    assert!(error.contains("deadline"), "{error}");
}
