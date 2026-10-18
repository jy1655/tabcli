use super::*;
use crate::native::query;
use crate::native::session::{CoreRecord, requests};

const SESSION: &str = "session-question";

fn question_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    PendingCodexTurn,
    Value,
    Observer,
) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(SESSION);
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    let store = Store::open_unchecked(&directory);
    store
        .record(CoreRecord::Manifest)
        .write_json(&json!({
            "schema":1,"id":SESSION,"provider":"codex","provider_path":"codex",
            "provider_version":"codex-cli 0.163.0","workspace":root.path(),
            "title":"question fixture","model":null,"effort":null,"yolo":true,"created_unix_ms":1
        }))
        .unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&store, &[]).unwrap();
    let pending = install_pending_turn(&directory, claim.token()).unwrap();
    claim.retain();
    let payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":THREAD,
        "turn_id":"turn-1", "transcript_path":directory.join("rollout.jsonl"),
        "prompt":correlated_prompt(&crate::native::native_delegation_prompt("external", "test"), &pending)});
    ADAPTER.handle_hook(&directory, &payload).unwrap();
    append(&directory.join("rollout.jsonl"), &prefix(&payload));
    (root, directory, pending, payload, Observer::default())
}

fn question_item(id: &str, questions: Value) -> Value {
    json!({"type":"item_completed","thread_id":THREAD,"turn_id":"turn-1",
        "item":{"type":"AgentMessage","id":id,"delivery":"async","phase":"final_answer",
            "content":[{"type":"Text","text":"not projected"}],"questions":questions}})
}

fn request(directory: &Path, pending: &PendingCodexTurn) -> requests::Receipt {
    requests::for_claim(&Reader::open_unchecked(directory), &pending.claim_token)
        .unwrap()
        .unwrap()
}

#[test]
fn async_question_is_visible_through_public_queries_before_output_or_terminal() {
    let (root, directory, pending, _, mut observer) = question_fixture();
    let questions = json!([
        {"title":"Which report format? 한국어😀", "options":[" Text ","Table"]},
        {"title":"Report heading?", "options":null}
    ]);
    append(
        &directory.join("rollout.jsonl"),
        &line(
            "event_msg",
            question_item("call_example", questions.clone()),
        ),
    );
    observer.poll(&directory, false);
    assert_pending(&directory, &pending);
    let receipt = request(&directory, &pending);
    let result =
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(result["request_state"], "pending");
    assert_eq!(result["result"], Value::Null);
    assert_eq!(
        result["question_emissions"][0]["questions"][0]["title"],
        questions[0]["title"]
    );
    assert_eq!(
        result["question_emissions"],
        json!([{"id":"call_example","questions":questions}])
    );
    let status = query::status::test_value_in(root.path());
    assert_eq!(
        status["sessions"][0]["active_request"]["request_id"],
        receipt.request_id
    );
    assert!(
        status["sessions"][0]["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    );
    let inspect = query::inspect_value(&Reader::open_unchecked(&directory), SESSION).unwrap();
    assert_eq!(inspect["requests"][0]["question_emitted"], true);
    assert_pending(&directory, &pending);
}
