//! Retained question presence on the outputs of an ended Request: a count without content,
//! read-only, and no question record read by a pending poll without `--questions`.
use super::*;
use crate::native::session::test_support::write_test_manifest;
use crate::native::session::{SessionState, Store, turn, update_status};

const SESSION: &str = "session-questions";

fn session(root: &Path) -> (PathBuf, turn::Claim) {
    let directory = root.join(SESSION);
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory, "codex");
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    (directory, claim)
}

fn record(directory: &Path, receipt: &requests::Receipt, incomplete: bool) {
    fs::write(
        directory.join(format!("codex-questions-{}.json", receipt.claim_token)),
        serde_json::to_vec_pretty(&json!({
            "schema": 1, "request_id": receipt.request_id, "claim_token": receipt.claim_token,
            "items": [
                {"id": "call_a", "questions": [{"title": "Which baseline path?", "options": null}]},
                {"id": "call_b", "questions": [{"title": "Format?", "options": ["Text", "Table"]}]}
            ],
            "incomplete": incomplete
        }))
        .unwrap(),
    )
    .unwrap();
}

fn complete(directory: &Path, token: &str) {
    turn::Report::for_claim(
        &Store::open_unchecked(directory),
        FirstPartyCli::Codex,
        Some(token),
    )
    .complete("answer", Some("thread".into()), Some("turn".into()))
    .unwrap();
}

fn tree(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.extend(tree(&path));
        } else {
            entries.push((path.clone(), fs::read(&path).unwrap()));
        }
    }
    entries.sort();
    entries
}

#[test]
fn ended_outputs_count_retained_emissions_without_content() {
    let root = tempfile::tempdir().unwrap();
    let (directory, claim) = session(root.path());
    let receipt = claim.receipt().clone();
    let token = claim.token().to_owned();
    claim.retain();
    record(&directory, &receipt, false);
    complete(&directory, &token);
    let address = format!("{SESSION}/{}", receipt.request_id);
    let before = tree(root.path());
    let waited = wait::one(
        root.path(),
        SESSION,
        &receipt.request_id,
        Duration::from_secs(1),
    )
    .unwrap();
    let default = wait::test_value_in(root.path(), &[&address], Duration::from_secs(1)).unwrap();
    let opted = wait::test_value_in(
        root.path(),
        &[&address, "--questions"],
        Duration::from_secs(1),
    )
    .unwrap();
    let finished = finished_question_presence(&directory, &receipt.request_id);
    let result = request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(tree(root.path()), before, "queries must not write");
    for value in [&waited, &default["ended"], &opted["ended"], &result] {
        assert_eq!(value["ok"], true);
        assert_eq!(value["request_state"], "completed");
        assert_eq!(value["result"], "answer");
        assert_eq!(value["question_emitted"], true);
        assert_eq!(value["question_emission_count"], 2);
        assert!(value.get("question_observation").is_none());
    }
    // Default and result waits add presence only; content stays with result and --questions.
    for value in [&waited, &default["ended"]] {
        assert!(value.get("question_emissions").is_none());
        assert!(value.get("after_question").is_none());
    }
    assert!(default["question"].is_null());
    assert_eq!(default["remaining"], json!([]));
    assert_eq!(
        opted["ended"]["question_emissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(result["question_emissions"].as_array().unwrap().len(), 2);
    assert_eq!(
        Value::Object(finished),
        json!({"question_emitted": true, "question_emission_count": 2})
    );
    assert_eq!(
        question_lines(&waited),
        [
            "question emissions retained: 2".to_owned(),
            format!(
                "  show them: {PUBLIC_COMMAND} result {SESSION} --request {} --json",
                receipt.request_id
            ),
        ]
    );
    let shown = question_lines(&result);
    assert_eq!(shown[0], "question emissions retained: 2");
    assert!(!shown.iter().any(|line| line.contains("show them")));
    assert!(shown.contains(&"question emitted: Format?".to_owned()));
}

#[test]
fn a_pending_poll_without_questions_reads_no_question_record() {
    let root = tempfile::tempdir().unwrap();
    let (directory, claim) = session(root.path());
    let receipt = claim.receipt().clone();
    let token = claim.token().to_owned();
    claim.retain();
    fs::write(
        directory.join(format!("codex-questions-{}.json", receipt.claim_token)),
        b"not a question record",
    )
    .unwrap();
    let pending = wait::one(root.path(), SESSION, &receipt.request_id, Duration::ZERO).unwrap();
    assert_eq!(pending["request_state"], "pending");
    assert_eq!(pending["timed_out"], true);
    assert!(pending.get("question_observation").is_none());
    // The ending reads it once: unreadable evidence is reported and changes no outcome.
    complete(&directory, &token);
    let ended = wait::one(
        root.path(),
        SESSION,
        &receipt.request_id,
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(ended["ok"], true);
    assert_eq!(ended["request_state"], "completed");
    assert_eq!(ended["question_observation"], "unreadable");
    assert!(ended.get("question_emission_count").is_none());
    assert!(
        finished_question_presence(&directory, &receipt.request_id)
            .contains_key("question_observation")
    );
}

#[test]
fn status_counts_the_focus_request_and_closed_sessions_keep_no_attention() {
    let has_attention = |session: &Value| {
        session["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("question_emitted"))
    };
    let root = tempfile::tempdir().unwrap();
    let (directory, claim) = session(root.path());
    let receipt = claim.receipt().clone();
    let token = claim.token().to_owned();
    claim.retain();
    record(&directory, &receipt, true);
    let pending = status::test_value_in(root.path())["sessions"][0].clone();
    assert_eq!(pending["active_request"]["request_id"], receipt.request_id);
    assert_eq!(pending["active_request"]["question_emitted"], true);
    assert_eq!(pending["active_request"]["question_emission_count"], 2);
    assert_eq!(pending["question_observation"], "incomplete");
    assert!(has_attention(&pending));
    complete(&directory, &token);
    update_status(&directory, SessionState::Closed, None, None).unwrap();
    let closed = status::test_value_in(root.path())["sessions"][0].clone();
    assert_eq!(closed["state"], "closed");
    assert!(closed["active_request"].is_null());
    assert_eq!(closed["latest_result"]["request_id"], receipt.request_id);
    assert_eq!(closed["latest_result"]["question_emission_count"], 2);
    assert!(!has_attention(&closed));
}

#[test]
fn status_keeps_question_evidence_when_the_latest_event_disappears_before_decode() {
    let root = tempfile::tempdir().unwrap();
    let (directory, claim) = session(root.path());
    let receipt = claim.receipt().clone();
    let token = claim.token().to_owned();
    claim.retain();
    record(&directory, &receipt, true);
    complete(&directory, &token);
    let (value, reads) = with_event_read_count(|| {
        with_event_read_hook(
            |path| fs::remove_file(path).unwrap(),
            || status::test_value_in(root.path())["sessions"][0].clone(),
        )
    });
    assert_eq!(reads, 1);
    assert!(value["active_request"].is_null());
    assert!(value["latest_result"].is_null());
    assert!(value["result_command"].is_null());
    assert_eq!(value["question_observation"], "incomplete");
    let attention = value["attention"].as_array().unwrap();
    assert!(attention.contains(&json!("question_emitted")));
    assert!(!attention.contains(&json!("records_partially_unreadable")));
    assert!(value.get("question_emission_count").is_none());
    assert!(value.get("question_emissions").is_none());
}
