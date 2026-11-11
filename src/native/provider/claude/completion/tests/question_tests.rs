use super::*;
use crate::native::{provider, query, session::requests};
use std::{collections::BTreeMap, path::PathBuf};

fn questions(step: u64, label: &str) -> Value {
    json!({"turnId":TURN,"step":step,"items":[{"index":0,"questions":[
        {"question":label,"header":"Baseline","multiSelect":false,
         "options":[{"label":"Current","description":"Current baseline"},
                    {"label":"Previous","description":"Previous baseline"}]}
    ]}],"incomplete":false})
}

fn receipt(f: &Fixture) -> requests::Receipt {
    requests::for_claim(&f.store(), &f.token).unwrap().unwrap()
}

fn observe(f: &Fixture) -> Option<provider::QuestionEmissions> {
    let store = f.store();
    let _lock = store.lock().unwrap();
    provider::question_emissions(&store, "claude", true, &receipt(f))
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            result.extend(snapshot(&entry.path()));
        } else {
            result.insert(entry.path(), std::fs::read(entry.path()).unwrap());
        }
    }
    result
}

#[test]
fn claude_questions_wake_public_wait_with_read_only_cursors_and_historical_results() {
    let root = tempfile::tempdir().unwrap();
    let f = Fixture::at(
        tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(root.path())
            .unwrap(),
    );
    let session = f.root.path().file_name().unwrap().to_str().unwrap();
    f.store()
        .record(CoreRecord::Manifest)
        .write_json(&json!({
            "schema":1,"id":session,"provider":"claude","provider_path":"claude",
            "provider_version":"2.1.309","workspace":root.path(),"title":"question fixture",
            "model":null,"effort":null,"yolo":true,"created_unix_ms":1
        }))
        .unwrap();
    f.start();
    f.send("question.emissions", questions(0, "Which baseline?"));
    let receipt = receipt(&f);
    let address = format!("{session}/{}", receipt.request_id);
    let before = snapshot(root.path());
    let result = query::request_result(&f.store(), &receipt.request_id).unwrap();
    assert_eq!(result["request_state"], "pending");
    assert_eq!(
        result["question_emissions"][0]["questions"][0]["title"],
        "Which baseline?"
    );
    let wait =
        |args: &[&str]| query::wait::test_value_in(root.path(), args, Duration::ZERO).unwrap();
    let first = wait(&[&address, "--questions"]);
    assert_eq!(first["ended"], Value::Null);
    assert_eq!(first["timed_out"], false);
    assert_eq!(first["remaining"], json!([address]));
    let cursor = first["question"]["after_question"].as_str().unwrap();
    assert_eq!(
        wait(&[&address, "--questions", "--after-question", cursor])["timed_out"],
        true
    );
    assert_eq!(wait(&[&address])["timed_out"], true);
    assert_eq!(
        query::inspect_value(&f.store(), session).unwrap()["requests"][0]["question_emitted"],
        true
    );
    assert_eq!(snapshot(root.path()), before);
    f.retained();

    f.send("question.emissions", questions(1, "Which comparison?"));
    let suffix = wait(&[&address, "--questions", "--after-question", cursor]);
    assert_eq!(
        suffix["question"]["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        suffix["question"]["question_emissions"][0]["id"],
        "step:1/tool:0"
    );
    f.complete_marked("done");
    let ended = wait(&[&address, "--questions", "--after-question", cursor]);
    assert_eq!(ended["ended"]["result"], "done");
    assert!(ended["question"].is_null());
    assert_eq!(observe(&f).unwrap().items.len(), 2);
}

#[test]
fn claude_questions_require_original_turn_and_never_follow_a_new_claim() {
    let f = Fixture::new();
    f.send("question.emissions", questions(0, "before binding"));
    assert!(observe(&f).is_none());
    f.start();
    for field in ["turnId", "agentId"] {
        let mut data = questions(0, "foreign");
        data[field] = json!("other");
        f.send("question.emissions", data);
    }
    let mut wire = f.wire("question.emissions", questions(0, "other conversation"));
    wire["conversation"] = json!("00000000-0000-4000-8000-000000000999");
    receive(f.root.path(), &wire).unwrap();
    assert!(observe(&f).is_none());
    f.send("question.emissions", questions(0, "original"));
    f.complete("unmarked interim");
    let before = snapshot(f.root.path());
    let mut continuation = questions(1, "unbound continuation");
    continuation["turnId"] = json!("next-turn");
    f.send("question.emissions", continuation);
    assert_eq!(snapshot(f.root.path()), before);
    assert_eq!(observe(&f).unwrap().items.len(), 1);
    f.complete_marked("done");
    let before = snapshot(f.root.path());
    f.send("question.emissions", questions(1, "late"));
    assert_eq!(snapshot(f.root.path()), before);
    let next = turn::claim(&f.store(), &[]).unwrap();
    let next_token = next.token().to_owned();
    next.retain();
    crate::native::session::update_status(f.root.path(), SessionState::Claimed, None, None)
        .unwrap();
    crate::native::session::update_status(f.root.path(), SessionState::Working, None, None)
        .unwrap();
    let next_message = ClaudeTurn::new("claude-turn-next")
        .unwrap()
        .target_message("[Agent Bridge native delegation]\nSource: external\n\nnext request");
    install_pending(
        f.root.path(),
        "claude-turn-next",
        Some(&next_token),
        &next_message,
    )
    .unwrap()
    .retain();
    f.send("question.emissions", questions(2, "late into next claim"));
    let receipt = requests::for_claim(&f.store(), &next_token)
        .unwrap()
        .unwrap();
    assert!(provider::question_emissions(&f.store(), "claude", true, &receipt).is_none());
    assert_eq!(observe(&f).unwrap().items.len(), 1);
}

#[test]
fn claude_questions_preserve_prefix_on_replay_conflict_invalid_and_oversized_items() {
    let f = Fixture::new();
    f.start();
    let first = questions(0, "first");
    f.send("question.emissions", first.clone());
    let before = snapshot(f.root.path());
    f.send("question.emissions", first);
    assert_eq!(snapshot(f.root.path()), before);
    f.send("question.emissions", questions(0, "conflict"));
    let mut invalid = questions(1, "invalid");
    invalid["items"][0]["questions"][0]["multiSelect"] = Value::Null;
    f.send("question.emissions", invalid);
    f.send("question.emissions", questions(2, "valid after malformed"));
    f.send("question.emissions", questions(1, "out of order"));
    let observation = observe(&f).unwrap();
    assert_eq!(observation.items.len(), 2);
    assert_eq!(observation.items[0].questions[0].title, "first");
    assert!(matches!(
        observation.observation,
        provider::QuestionObservation::Incomplete
    ));
    f.send("question.emissions", questions(3, &"x".repeat(65536)));
    f.send("question.emissions", questions(4, "after record is full"));
    assert_eq!(observe(&f).unwrap().items.len(), 2);
    f.retained();
    f.complete_marked("completion survives");
    assert_eq!(f.events()[0].message, "completion survives");
}

#[test]
fn claude_questions_damaged_records_do_not_change_completion_and_unlocked_reads_are_empty() {
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "first"));
    let receipt = receipt(&f);
    assert!(provider::question_emissions(&f.store(), "claude", false, &receipt).is_none());
    let path = questions_name(&f.token);
    f.store()
        .private(&path)
        .write_json(&json!("broken record"))
        .unwrap();
    assert!(matches!(
        observe(&f).unwrap().observation,
        provider::QuestionObservation::Unreadable
    ));
    assert!(
        receive(
            f.root.path(),
            &f.wire("question.emissions", questions(1, "second"))
        )
        .is_err()
    );
    f.retained();
    f.complete_marked("still completes");
    assert_eq!(f.events()[0].message, "still completes");
    assert_eq!(
        f.store().private(&path).json::<Value>().unwrap(),
        json!("broken record")
    );
}

#[test]
fn claude_questions_ignore_closed_and_legacy_sessions() {
    for closed in [false, true] {
        let f = Fixture::new();
        f.start();
        if closed {
            crate::native::session::update_status(f.root.path(), SessionState::Closed, None, None)
                .unwrap();
        } else {
            f.store().private(MODE_FILE).remove().unwrap();
        }
        let before = snapshot(f.root.path());
        f.send("question.emissions", questions(0, "ignored"));
        assert_eq!(snapshot(f.root.path()), before);
        assert!(observe(&f).is_none());
    }
}

#[test]
fn claude_questions_malformed_envelopes_retain_no_emissions() {
    for (field, value) in [
        ("step", Value::Null),
        ("step", json!(-1)),
        ("step", json!(0.5)),
        ("items", json!({})),
        ("incomplete", Value::Null),
    ] {
        let f = Fixture::new();
        f.start();
        let mut data = questions(0, "invalid envelope");
        data[field] = value;
        f.send("question.emissions", data);
        let observation = observe(&f).unwrap();
        assert!(observation.items.is_empty());
        assert!(matches!(
            observation.observation,
            provider::QuestionObservation::Incomplete
        ));
        f.retained();
        f.complete_marked("completion is independent");
        assert_eq!(f.events()[0].message, "completion is independent");
    }
}

#[test]
fn claude_questions_limit_includes_the_entire_pretty_encoded_record() {
    for extra in [0, 1] {
        let f = Fixture::new();
        f.start();
        f.send("question.emissions", questions(0, "x"));
        let path = questions_name(&f.token);
        let baseline = f.store().private(&path).text().unwrap().unwrap().len();
        f.store().private(&path).remove().unwrap();
        f.send(
            "question.emissions",
            questions(0, &"x".repeat(65536 - baseline + 1 + extra)),
        );
        let observation = observe(&f).unwrap();
        if extra == 0 {
            assert_eq!(
                f.store().private(&path).text().unwrap().unwrap().len(),
                65536
            );
            assert_eq!(observation.items.len(), 1);
            assert!(matches!(
                observation.observation,
                provider::QuestionObservation::Complete
            ));
        } else {
            assert!(observation.items.is_empty());
            assert!(matches!(
                observation.observation,
                provider::QuestionObservation::Incomplete
            ));
            assert_eq!(
                f.store().private(&path).json::<Value>().unwrap()["full"],
                true
            );
        }
        f.retained();
    }
}

// The refused call's own questions, in the shape the emission of `questions(step, label)` has.
fn deflected(step: u64, index: u64, label: &str) -> Value {
    json!({"turnId":TURN,"step":step,"index":index,
        "questions":questions(step, label)["items"][0]["questions"]})
}

fn deflected_ids(f: &Fixture) -> Vec<String> {
    observe(f)
        .unwrap()
        .items
        .into_iter()
        .filter(|item| item.deflected)
        .map(|item| item.id)
        .collect()
}

#[test]
fn claude_deflection_marks_only_a_retained_item_once_and_never_settles() {
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "first"));
    let path = questions_name(&f.token);
    // A record without the flag (any record written before deflection existed) reads as not
    // deflected, and an item that is not deflected is written without the field.
    assert!(
        !f.store()
            .private(&path)
            .text()
            .unwrap()
            .unwrap()
            .contains("deflected")
    );
    assert!(deflected_ids(&f).is_empty());
    f.send("question.deflected", deflected(0, 0, "first"));
    assert_eq!(deflected_ids(&f), ["step:0/tool:0"]);
    assert!(matches!(
        observe(&f).unwrap().observation,
        provider::QuestionObservation::Complete
    ));
    let saved: Value = f.store().private(&path).json().unwrap();
    assert_eq!(saved["items"][0]["deflected"], true);
    // Repeats and a replay of the same emission change nothing; the flag is not a conflict.
    let before = snapshot(f.root.path());
    f.send("question.deflected", deflected(0, 0, "first"));
    f.send("question.emissions", questions(0, "first"));
    assert_eq!(snapshot(f.root.path()), before);
    // A later emission is retained unflagged; the earlier flag stays.
    f.send("question.emissions", questions(1, "second"));
    assert_eq!(deflected_ids(&f), ["step:0/tool:0"]);
    assert_eq!(observe(&f).unwrap().items.len(), 2);
    // An item the record lacks is never invented: the observation becomes incomplete.
    f.send("question.deflected", deflected(2, 0, "first"));
    let observation = observe(&f).unwrap();
    assert_eq!(observation.items.len(), 2);
    assert!(matches!(
        observation.observation,
        provider::QuestionObservation::Incomplete
    ));
    assert_eq!(deflected_ids(&f), ["step:0/tool:0"]);
    f.retained();
    f.complete_marked("QUESTION FOR CALLER: Which baseline? Options: Current, Previous");
    assert_eq!(
        f.events()[0].message,
        "QUESTION FOR CALLER: Which baseline? Options: Current, Previous"
    );
    assert_eq!(deflected_ids(&f), ["step:0/tool:0"]);
}

#[test]
fn claude_deflection_without_a_record_or_with_malformed_data_marks_only_incomplete() {
    for data in [
        deflected(0, 0, "first"),
        // A repeated step can retain another question at the refused call's position.
        deflected(0, 0, "another"),
        json!({"turnId":TURN,"step":0,"index":0}),
        json!({"turnId":TURN,"step":0,"index":0,"questions":[]}),
        json!({"turnId":TURN,"step":0}),
        json!({"turnId":TURN,"step":Value::Null,"index":0}),
        json!({"turnId":TURN,"step":-1,"index":0}),
        json!({"turnId":TURN,"step":0,"index":0.5}),
        json!({"turnId":TURN,"step":0,"index":"0"}),
    ] {
        let f = Fixture::new();
        f.start();
        // The first case has no record at all; every other case has the item "first" retained.
        let first = data == deflected(0, 0, "first");
        if !first {
            f.send("question.emissions", questions(0, "first"));
        }
        f.send("question.deflected", data.clone());
        let observation = observe(&f).unwrap();
        assert_eq!(observation.items.len(), usize::from(!first), "{data}");
        assert!(
            observation.items.iter().all(|item| !item.deflected),
            "{data}"
        );
        assert!(
            matches!(
                observation.observation,
                provider::QuestionObservation::Incomplete
            ),
            "{data}"
        );
        f.retained();
    }
}

#[test]
fn claude_deflection_requires_the_original_turn_and_a_live_session() {
    let f = Fixture::new();
    f.send("question.deflected", deflected(0, 0, "original"));
    assert!(observe(&f).is_none(), "before the binding");
    f.start();
    f.send("question.emissions", questions(0, "original"));
    let before = snapshot(f.root.path());
    let mut other_turn = deflected(0, 0, "original");
    other_turn["turnId"] = json!("next-turn");
    f.send("question.deflected", other_turn);
    let mut subagent = deflected(0, 0, "original");
    subagent["agentId"] = json!("child");
    f.send("question.deflected", subagent);
    let mut wire = f.wire("question.deflected", deflected(0, 0, "original"));
    wire["conversation"] = json!("00000000-0000-4000-8000-000000000999");
    receive(f.root.path(), &wire).unwrap();
    assert_eq!(snapshot(f.root.path()), before);
    crate::native::session::update_status(f.root.path(), SessionState::Closed, None, None).unwrap();
    let before = snapshot(f.root.path());
    f.send("question.deflected", deflected(0, 0, "original"));
    assert_eq!(snapshot(f.root.path()), before);
    assert!(deflected_ids(&f).is_empty());
}

#[test]
fn claude_deflection_that_would_exceed_the_record_limit_marks_the_record_full() {
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "x"));
    let path = questions_name(&f.token);
    let baseline = f.store().private(&path).text().unwrap().unwrap().len();
    f.store().private(&path).remove().unwrap();
    // The largest record that fits: the flag no longer does.
    let label = "x".repeat(65536 - baseline + 1);
    f.send("question.emissions", questions(0, &label));
    assert_eq!(
        f.store().private(&path).text().unwrap().unwrap().len(),
        65536
    );
    f.send("question.deflected", deflected(0, 0, &label));
    let saved: Value = f.store().private(&path).json().unwrap();
    assert_eq!(saved["incomplete"], true);
    assert_eq!(saved["full"], true);
    assert!(saved["items"][0].get("deflected").is_none());
    assert!(f.store().private(&path).text().unwrap().unwrap().len() <= 65536);
    assert!(matches!(
        observe(&f).unwrap().observation,
        provider::QuestionObservation::Incomplete
    ));
    f.retained();
}

#[test]
fn claude_questions_reject_mismatched_historical_identity_and_invalid_prefix() {
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "first"));
    let path = questions_name(&f.token);
    let saved: Value = f.store().private(&path).json().unwrap();
    for pointer in ["/request_id", "/binding/claim", "/items/0/id", "/full"] {
        let mut damaged = saved.clone();
        *damaged.pointer_mut(pointer).unwrap() = if pointer == "/full" {
            json!(true)
        } else {
            json!("other")
        };
        f.store().private(&path).write_json(&damaged).unwrap();
        let before = snapshot(f.root.path());
        assert!(matches!(
            observe(&f).unwrap().observation,
            provider::QuestionObservation::Unreadable
        ));
        assert_eq!(snapshot(f.root.path()), before);
    }
    f.complete_marked("completion is independent");
    assert_eq!(f.events()[0].message, "completion is independent");
}

// The nested binding of a question record meets the binding floor for the Receipt's Claim and
// carries any valid correlation identity. A record below it is unreadable to the query and
// refused by the next emission with the reader's text; neither changes the record.
#[test]
fn claude_question_record_binding_keeps_its_own_correlation_check() {
    for (pointer, value) in [
        ("/schema", json!(2)),
        ("/binding/schema", json!(2)),
        ("/binding/claim", json!("999-999-999")),
        ("/binding/conversation", json!("not-a-conversation")),
        ("/binding/turn_id", json!("../invalid")),
        ("/binding/correlation_id", json!("../invalid")),
        ("/binding/correlation_id", json!("")),
    ] {
        let f = Fixture::new();
        f.start();
        f.send("question.emissions", questions(0, "first"));
        let path = questions_name(&f.token);
        let mut saved: Value = f.store().private(&path).json().unwrap();
        *saved.pointer_mut(pointer).unwrap() = value;
        f.store().private(&path).write_json(&saved).unwrap();
        let before = f.store().private(&path).text().unwrap();
        assert!(
            matches!(
                observe(&f).unwrap().observation,
                provider::QuestionObservation::Unreadable
            ),
            "{pointer}"
        );
        let error = receive(
            f.root.path(),
            &f.wire("question.emissions", questions(1, "second")),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "invalid Claude question record",
            "{pointer}"
        );
        assert_eq!(
            f.store().private(&path).text().unwrap(),
            before,
            "{pointer}"
        );
        f.retained();
    }
    // A valid correlation identity other than the pending request's is read, never replaced.
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "first"));
    let path = questions_name(&f.token);
    let mut saved: Value = f.store().private(&path).json().unwrap();
    saved["binding"]["correlation_id"] = json!("claude-turn-other");
    f.store().private(&path).write_json(&saved).unwrap();
    let before = f.store().private(&path).text().unwrap();
    let observation = observe(&f).unwrap();
    assert_eq!(observation.items.len(), 1);
    assert!(matches!(
        observation.observation,
        provider::QuestionObservation::Complete
    ));
    for event in ["question.emissions", "question.deflected"] {
        let data = if event == "question.emissions" {
            questions(1, "second")
        } else {
            deflected(0, 0, "first")
        };
        let error = receive(f.root.path(), &f.wire(event, data)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "Claude question binding cannot be replaced",
            "{event}"
        );
    }
    assert_eq!(f.store().private(&path).text().unwrap(), before);
    f.retained();
}

// The question record is read within 64 KiB of strict UTF-8 and its prefix is validated; each
// refusal keeps its text, and the decoder's text names the record's struct.
#[test]
fn claude_question_record_refusals_keep_their_texts() {
    let f = Fixture::new();
    f.start();
    f.send("question.emissions", questions(0, "first"));
    let path = questions_name(&f.token);
    let saved = f.store().private(&path).text().unwrap().unwrap();
    let file = f.root.path().join(&path);
    let mut oversized = saved.trim_end().strip_suffix('}').unwrap().to_owned();
    oversized.push_str(&format!(",\"pad\":\"{}\"}}", "p".repeat(65536)));
    let oversized_len = oversized.len();
    let mut not_utf8 = saved
        .trim_end()
        .strip_suffix('}')
        .unwrap()
        .as_bytes()
        .to_vec();
    not_utf8.extend_from_slice(b",\"pad\":\"\xff\"}");
    let utf8_error = std::str::from_utf8(&not_utf8).unwrap_err();
    let edited = |pointer: &str, value: Value| {
        let mut record: Value = serde_json::from_str(&saved).unwrap();
        *record.pointer_mut(pointer).unwrap() = value;
        serde_json::to_vec_pretty(&record).unwrap()
    };
    for (bytes, expected) in [
        (
            br#""broken record""#.to_vec(),
            "invalid type: string \"broken record\", expected struct Record at line 1 column 15"
                .to_owned(),
        ),
        (
            edited("/request_id", json!("other")),
            "invalid Claude question record".to_owned(),
        ),
        (
            edited("/full", json!(true)),
            "invalid Claude question record".to_owned(),
        ),
        (
            edited("/items/0/id", json!("step:01/tool:0")),
            "invalid Claude question position".to_owned(),
        ),
        (
            edited("/items/0/questions", json!([])),
            "invalid Claude retained question prefix".to_owned(),
        ),
        (
            oversized.into_bytes(),
            format!("record is {oversized_len} bytes, over the 65536 byte read limit"),
        ),
        (not_utf8, format!("invalid UTF-8: {utf8_error}")),
    ] {
        std::fs::write(&file, &bytes).unwrap();
        assert!(
            matches!(
                observe(&f).unwrap().observation,
                provider::QuestionObservation::Unreadable
            ),
            "{expected}"
        );
        let error = receive(
            f.root.path(),
            &f.wire("question.emissions", questions(1, "second")),
        )
        .unwrap_err();
        assert_eq!(format!("{error:#}"), expected);
        assert_eq!(std::fs::read(&file).unwrap(), bytes, "{expected}");
    }
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let error = receive(
        f.root.path(),
        &f.wire("question.emissions", questions(1, "second")),
    )
    .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!("refusing non-regular session file: {}", file.display())
    );
    f.retained();
    f.complete_marked("completion is independent");
    assert_eq!(f.events()[0].message, "completion is independent");
}
