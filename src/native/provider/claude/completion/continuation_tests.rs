use super::*;

const CONTINUED: &str = "00000000-0000-4000-8000-000000000251";

#[test]
fn continuation_requires_a_valid_candidate_of_the_exact_current_binding() {
    for change in [
        "absent",
        "corrupt",
        "schema",
        "claim",
        "correlation",
        "conversation",
        "turn",
        "created",
        "preview",
    ] {
        let f = Fixture::new();
        f.start();
        f.complete("interim");
        let record = f.store().private(&candidate_name(&f.token));
        let mut value: Value = record.json().unwrap();
        match change {
            "absent" => record.remove().unwrap(),
            "corrupt" => {
                record.remove().unwrap();
                record.write_private(b"{").unwrap();
            }
            _ => {
                match change {
                    "schema" => value["schema"] = json!(2),
                    "claim" => value["binding"]["claim"] = json!("999-999-999"),
                    "correlation" => {
                        value["binding"]["correlation_id"] = json!("claude-turn-other")
                    }
                    "conversation" => {
                        value["binding"]["conversation"] =
                            json!("00000000-0000-0000-0000-000000000000")
                    }
                    "turn" => value["binding"]["turn_id"] = json!("wrong-turn"),
                    "created" => value["created_unix_ms"] = json!(0),
                    "preview" => value["answer_preview"] = json!("x".repeat(1025)),
                    _ => unreachable!(),
                }
                record.write_json(&value).unwrap();
            }
        }
        f.send("turn.complete", f.final_data(CONTINUED));
        f.retained();
        assert!(
            f.store()
                .private(DIAGNOSTIC_FILE)
                .text()
                .unwrap()
                .unwrap()
                .contains("candidate"),
            "{change}"
        );
    }
}

#[test]
fn unrelated_or_unattributable_continuations_cannot_settle_a_candidate() {
    for case in [
        "conversation",
        "marker",
        "unmarked",
        "infix",
        "subagent",
        "null agent",
        "aborted",
        "error",
        "refusal",
        "empty",
        "marker only",
        "invalid reason",
        "invalid abort",
        "invalid answer",
        "invalid refusal",
        "oversize",
        "bad identity",
    ] {
        let f = Fixture::new();
        f.start();
        f.complete("interim");
        let mut wire = f.wire("turn.complete", f.final_data(CONTINUED));
        match case {
            "conversation" => wire["conversation"] = json!("00000000-0000-0000-0000-000000000000"),
            "marker" => {
                wire["data"]["answer"] =
                    json!("final\n<!-- agent-bridge-claude-turn:claude-turn-other -->")
            }
            "unmarked" => wire["data"]["answer"] = json!("another interim"),
            "infix" => {
                wire["data"]["answer"] = json!(format!(
                    "{} extra",
                    wire["data"]["answer"].as_str().unwrap()
                ))
            }
            "subagent" => wire["data"]["agentId"] = json!("child"),
            "null agent" => wire["data"]["agentId"] = Value::Null,
            "aborted" => {
                wire["data"]["reason"] = json!("aborted");
                wire["data"]["isAborted"] = json!(true);
            }
            "error" => wire["data"]["reason"] = json!("error"),
            "refusal" => {
                wire["data"]["reason"] = json!("refusal");
                wire["data"]["refusal"] = json!({"category":"policy","explanation":"declined"});
            }
            "empty" => wire["data"]["answer"] = json!(" \n"),
            "marker only" => {
                wire["data"]["answer"] =
                    json!(format!("<!-- agent-bridge-claude-turn:{REQUEST} -->"))
            }
            "invalid reason" => wire["data"]["reason"] = json!("unknown"),
            "invalid abort" => wire["data"]["isAborted"] = json!(true),
            "invalid answer" => wire["data"]["answer"] = Value::Null,
            "invalid refusal" => wire["data"]["refusal"] = Value::Null,
            "oversize" => {
                wire["event"] = json!("oversize");
                wire["data"] = json!({"turnId":CONTINUED,"utf8_len":MAX_WIRE_BYTES+1});
            }
            "bad identity" => wire["data"]["turnId"] = json!("../wrong"),
            _ => unreachable!(),
        }
        receive(f.root.path(), &wire).unwrap();
        f.retained();
        assert!(
            f.store()
                .private(&format!("claude-mod-continuation-{}.json", f.token))
                .text()
                .unwrap()
                .is_none(),
            "{case}"
        );
    }
}

#[test]
fn candidate_does_not_outlive_claim_or_close_authority() {
    for close in [false, true] {
        let f = Fixture::new();
        f.start();
        f.complete("interim");
        if close {
            crate::native::session::update_status(f.root.path(), SessionState::Closed, None, None)
                .unwrap();
        } else {
            f.store().record(CoreRecord::TurnClaim).remove().unwrap();
            f.store()
                .record(CoreRecord::TurnClaim)
                .write_private(b"999-999-999")
                .unwrap();
        }
        f.send("turn.complete", f.final_data(CONTINUED));
        assert!(f.events().is_empty());
        if !close {
            assert_eq!(
                turn::current_claim_token(&f.store()).unwrap().as_deref(),
                Some("999-999-999")
            );
        }
    }
}

#[test]
fn unmarked_candidate_is_bounded_stable_and_survives_a_waiter_timeout() {
    let f = Fixture::new();
    f.start();
    f.complete(&"한😀".repeat(5000));
    let record = f.store().private(&candidate_name(&f.token));
    let before = record.text().unwrap();
    let candidate: Candidate = record.json().unwrap();
    assert_eq!(candidate.answer_preview.chars().count(), 1024);
    f.complete("duplicate must not replace the first observation");
    assert_eq!(record.text().unwrap(), before);
    assert!(
        turn::wait(
            &f.store(),
            0,
            Some(REQUEST),
            Some(&f.token),
            Instant::now(),
            Duration::ZERO
        )
        .is_err()
    );
    f.retained();
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(f.events()[0].message, "final result");
    let selected: Continuation = f
        .store()
        .private(&format!("claude-mod-continuation-{}.json", f.token))
        .json()
        .unwrap();
    assert_eq!(selected.binding.turn_id, TURN);
    assert_eq!(selected.turn_id, CONTINUED);
    assert_eq!(selected.binding.correlation_id, REQUEST);
}

#[test]
fn a_selected_continuation_cannot_be_replaced_before_publication() {
    let f = Fixture::new();
    f.start();
    f.complete("interim");
    let binding: Binding = f
        .store()
        .private(&format!("claude-mod-bound-{}.json", f.token))
        .json()
        .unwrap();
    let selected = select_continuation(&f.store(), &binding, CONTINUED)
        .unwrap()
        .unwrap();
    let record = f
        .store()
        .private(&format!("claude-mod-continuation-{}.json", f.token));
    // Deterministic interruption after selection, before core publication.
    record.write_json(&selected).unwrap();
    let before = record.text().unwrap();
    f.send("turn.complete", f.final_data("different-turn"));
    f.retained();
    assert_eq!(record.text().unwrap(), before);
    f.send("turn.complete", f.final_data(CONTINUED));
    f.send("turn.complete", f.final_data(CONTINUED));
    assert_eq!(f.events().len(), 1);
    assert_eq!(record.text().unwrap(), before);
}

#[test]
fn malformed_bound_completion_creates_no_candidate_for_an_unbound_final() {
    let f = Fixture::new();
    f.start();
    f.send(
        "turn.complete",
        json!({"turnId":TURN,"reason":"answer","isAborted":true,"answer":"interim"}),
    );
    assert!(
        f.store()
            .private(&candidate_name(&f.token))
            .text()
            .unwrap()
            .is_none()
    );
    f.send("turn.complete", f.final_data(CONTINUED));
    f.retained();
}

#[test]
fn damaged_selection_is_not_repaired_by_completion_or_inspection() {
    for change in ["corrupt", "schema", "claim", "turn", "created"] {
        let f = Fixture::new();
        f.start();
        f.complete("interim");
        let binding: Binding = f
            .store()
            .private(&format!("claude-mod-bound-{}.json", f.token))
            .json()
            .unwrap();
        let selected = select_continuation(&f.store(), &binding, CONTINUED)
            .unwrap()
            .unwrap();
        let record = f
            .store()
            .private(&format!("claude-mod-continuation-{}.json", f.token));
        let mut value = serde_json::to_value(selected).unwrap();
        match change {
            "corrupt" => record.write_private(b"{").unwrap(),
            _ => {
                match change {
                    "schema" => value["schema"] = json!(2),
                    "claim" => value["binding"]["claim"] = json!("999-999-999"),
                    "turn" => value["turn_id"] = json!("../invalid"),
                    "created" => value["created_unix_ms"] = json!(0),
                    _ => unreachable!(),
                }
                record.write_json(&value).unwrap();
            }
        }
        let before = record.text().unwrap();
        f.send("turn.complete", f.final_data(CONTINUED));
        f.retained();
        let store = f.store();
        let _lock = store.lock().unwrap();
        let observation = observe_candidate(&store, &f.token).unwrap();
        assert_eq!(observation["state"], "unreadable", "{change}");
        assert_eq!(record.text().unwrap(), before, "{change}");
    }
}

#[test]
fn candidate_observation_does_not_route_other_providers_or_unlocked_reads() {
    let f = Fixture::new();
    f.start();
    f.complete("interim");
    let store = f.store();
    let _lock = store.lock().unwrap();
    for (provider, locked, claim) in [
        ("codex", true, Some(f.token.as_str())),
        ("pi", true, Some(f.token.as_str())),
        ("agy", true, Some(f.token.as_str())),
        ("claude", false, Some(f.token.as_str())),
        ("claude", true, None),
    ] {
        assert!(
            crate::native::provider::completion_observation(&store, provider, locked, claim)
                .is_none()
        );
    }
    assert_eq!(
        observe_candidate(&store, &f.token).unwrap()["state"],
        "unmarked_bound_turn"
    );
}

#[test]
fn bound_failure_after_candidate_still_settles_and_rejects_a_later_final() {
    for reason in ["aborted", "error", "refusal", "answer"] {
        let f = Fixture::new();
        f.start();
        f.complete("interim");
        let mut data = json!({"turnId":TURN,"reason":reason,
            "isAborted":reason == "aborted","answer":""});
        if reason == "refusal" {
            data["refusal"] = json!({"category":null,"explanation":null});
        }
        f.send("turn.complete", data);
        f.send("turn.complete", f.final_data(CONTINUED));
        let events = f.events();
        assert_eq!(events.len(), 1, "{reason}");
        assert!(events[0].error.is_some(), "{reason}");
        assert_eq!(events[0].turn_id.as_deref(), Some(REQUEST));
    }
}
