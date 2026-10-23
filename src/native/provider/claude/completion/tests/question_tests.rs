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
    let mut continuation = questions(1, "unbound continuation");
    continuation["turnId"] = json!("next-turn");
    f.send("question.emissions", continuation);
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
    let path = format!("claude-questions-{}.json", f.token);
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
