use super::super::test_support::{
    claim_pending_turn, go_quoted, marked, parse_transcript_line, planner_line, receipt_line,
};
use super::super::{PENDING_TURN_FILE, PendingAgyTurn, terminal_correlated_prompt};
use super::*;
use crate::native::session::SessionState;
use crate::native::session::schema::SessionEvent;
use crate::native::{SessionStatus, event_paths, read_json, update_status};
use std::fs::{self, OpenOptions};
use std::io::Write;

#[test]
fn log_and_transcript_parsers_accept_only_completed_results() {
    let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
    assert_eq!(
        parse_conversation_id(&format!("prefix Created conversation {id}\n")),
        Some(id.to_owned())
    );
    assert!(parse_conversation_id("Created conversation ../../outside").is_none());

    let completed = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":9,"content":"AGY_TOOL_OK"}"#;
    assert_eq!(
        parse_transcript_line(completed),
        Some((9, "AGY_TOOL_OK".to_owned()))
    );
    let intermediate = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":7,"content":""}"#;
    assert_eq!(parse_transcript_line(intermediate), None);
    let planner_tool = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":8,"content":"checking","tool_calls":[{"name":"run_command"}]}"#;
    assert_eq!(parse_transcript_line(planner_tool), None);
    let tool = r#"{"type":"RUN_COMMAND","status":"DONE","source":"MODEL","step_index":8,"content":"output"}"#;
    assert_eq!(parse_transcript_line(tool), None);
}

#[test]
fn transcript_cursor_records_each_completed_response_once() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();

    let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
    let brain = root.path().join("brain");
    let transcript_path = brain
        .join(id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    let first_pending = claim_pending_turn(&directory);
    let transcript = [
        planner_line(1, &marked("first", &first_pending)),
        serde_json::json!({
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "source": "MODEL",
            "step_index": 2,
            "content": "still working",
            "tool_calls": [{"name": "run_command"}],
        })
        .to_string(),
        serde_json::json!({
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "source": "MODEL",
            "step_index": 3,
            "content": "short...",
            "is_truncated": true,
        })
        .to_string(),
    ]
    .join("\n")
        + "\n";
    fs::write(&transcript_path, transcript).unwrap();
    let mut cursor = TranscriptCursor::new(transcript_path.clone());
    cursor.poll(&directory, &brain, id).unwrap();
    cursor.poll(&directory, &brain, id).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 1);
    assert!(directory.join(PENDING_TURN_FILE).is_file());

    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let second_pending = claim_pending_turn(&directory);
    let full_path = transcript_path.with_file_name("transcript_full.jsonl");
    let results = ResultReader {
        full_path: &full_path,
        brain_root: &brain,
    };
    let truncated = PlannerResult {
        step: 3,
        message: "short...".to_owned(),
        truncated: true,
    };
    assert!(matches!(
        results
            .observe(&truncated, || panic!(
                "incomplete evidence must not bind a turn"
            ))
            .unwrap(),
        ResultEvidence::Incomplete
    ));
    assert!(
        !results
            .contains(std::slice::from_ref(&transcript_path), &second_pending)
            .unwrap()
    );
    fs::write(
        transcript_path.with_file_name("transcript_full.jsonl"),
        format!(
            "{}\n{}\n",
            planner_line(1, &marked("first", &first_pending)),
            planner_line(3, &marked("complete long response", &second_pending)),
        ),
    )
    .unwrap();
    let before = fs::read(directory.join("status.json")).unwrap();
    assert!(
        results
            .contains(std::slice::from_ref(&transcript_path), &second_pending)
            .unwrap()
    );
    assert_eq!(fs::read(directory.join("status.json")).unwrap(), before);
    assert_eq!(event_paths(&directory).unwrap().len(), 1);
    cursor.poll(&directory, &brain, id).unwrap();
    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 2);
    let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
    assert_eq!(latest.message, "complete long response");

    let mut transcript = OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .unwrap();
    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let third_pending = claim_pending_turn(&directory);
    writeln!(
        transcript,
        "{}",
        planner_line(4, &marked("second", &third_pending))
    )
    .unwrap();
    cursor.poll(&directory, &brain, id).unwrap();

    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 3);
    let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
    assert_eq!(latest.message, "second");
    assert_eq!(latest.provider_session_id.as_deref(), Some(id));
    assert_eq!(latest.turn_id.as_deref(), Some("4"));
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
}

#[test]
fn monitor_switches_to_the_newest_created_conversation() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let brain = root.path().join("brain");
    let log = directory.join("agy.log");
    let first_id = "11111111-1111-1111-1111-111111111111";
    let second_id = "22222222-2222-2222-2222-222222222222";
    for id in [first_id, second_id] {
        let transcript = brain
            .join(id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(transcript, "").unwrap();
    }
    let first_pending = claim_pending_turn(&directory);
    fs::write(
        brain
            .join(first_id)
            .join(".system_generated/logs/transcript.jsonl"),
        format!(
            "{}\n",
            planner_line(1, &marked("before clear", &first_pending))
        ),
    )
    .unwrap();
    fs::write(&log, format!("Created conversation {first_id}\n")).unwrap();
    let mut monitor = MonitorState::default();

    monitor.poll(&directory, &log, &brain).unwrap();
    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let second_pending = claim_pending_turn(&directory);
    fs::write(
        brain
            .join(second_id)
            .join(".system_generated/logs/transcript.jsonl"),
        format!(
            "{}\n",
            planner_line(1, &marked("after clear", &second_pending))
        ),
    )
    .unwrap();
    fs::write(
        &log,
        format!("Created conversation {first_id}\n/clear\nCreated conversation {second_id}\n"),
    )
    .unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();

    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 2);
    let first: SessionEvent = read_json(&paths[0]).unwrap();
    let second: SessionEvent = read_json(&paths[1]).unwrap();
    assert_eq!(first.message, "before clear");
    assert_eq!(first.provider_session_id.as_deref(), Some(first_id));
    assert_eq!(second.message, "after clear");
    assert_eq!(second.provider_session_id.as_deref(), Some(second_id));
    assert_eq!(second.turn_id.as_deref(), Some("1"));
}

// A turn Agy gives up on is recorded as a failed request at once instead of
// waiting out the timeout: the quota error of 2026-10-01 (session-UuYk87), with
// its log lines verbatim. The failing turn must be the pending one by the log's
// own account: the first, argument-delivered turn whose only transcript input
// carries the marker, or a pasted turn behind its receipt. An error of a turn
// typed by hand, of an older turn, or of a first turn that a newer pending claim
// has not replaced in the log yet is ignored. A result written before the error
// is seen wins; one written after the failure was recorded is not accepted.
#[test]
fn monitor_records_a_turn_agy_gave_up_on_as_a_failed_request() {
    use super::super::super::super::TURN_CLAIM_FILE;
    const FORWARDED: &str = "I1001 17:23:48.670594     402 conversation_manager.go:699] Forwarding user message to conversation 97ad12fd-9e7a-4556-83a4-8f0147343657 (items=1, media=0)\n";
    const QUOTA: &str = "E1001 17:23:49.333283     246 errorreport.go:224] agent executor error: generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h22m28s.\nE1001 17:23:49.335578     246 errorreport.go:224] generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h22m28s.\n";
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-quota1");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let brain = root.path().join("brain");
    let log = directory.join("agy.log");
    let id = "97ad12fd-9e7a-4556-83a4-8f0147343657";
    let logs = brain.join(id).join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join("transcript.jsonl"), "").unwrap();
    let full = logs.join("transcript_full.jsonl");
    let user_input = |step: u64, text: &str| {
        serde_json::json!({
            "step_index": step,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "content": format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>"),
        })
        .to_string()
            + "\n"
    };
    let pasted = |pending: &PendingAgyTurn| {
        receipt_line(&go_quoted(
            &terminal_correlated_prompt("again", pending, false).unwrap(),
        ))
    };
    let created =
        format!("I1001 17:23:46.491183     402 server.go:1248] Created conversation {id}\n");
    let first = claim_pending_turn(&directory);
    let mut monitor = MonitorState::default();
    let quota_failure = |event: &SessionEvent| {
        assert_eq!(event.message, "");
        let error = event.error.clone().unwrap();
        assert!(
            error.starts_with("Agy turn failed: generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached."),
            "{error}"
        );
        assert!(error.ends_with("Resets in 2h22m28s."), "{error}");
        assert_eq!(event.provider_session_id.as_deref(), Some(id));
        error
    };

    // The first turn failed, but Agy's record of its input does not carry this
    // claim's marker: nothing is attributed.
    fs::write(&full, user_input(0, "another request")).unwrap();
    fs::write(&log, created.clone() + FORWARDED + QUOTA).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    assert!(event_paths(&directory).unwrap().is_empty());
    assert!(directory.join(TURN_CLAIM_FILE).exists());

    // The argument-delivered first turn is the pending one, and Agy gave up on it.
    fs::write(&full, user_input(0, &marked("request", &first))).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 1, "the failure is recorded once");
    let error = quota_failure(&read_json(&paths[0]).unwrap());
    assert!(
        !directory.join(TURN_CLAIM_FILE).exists(),
        "the failed turn releases its claim"
    );
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
    assert_eq!(status.error.as_deref(), Some(error.as_str()));

    // A follow-up is claimed and its input already stands in the transcript, but
    // the log still ends with the first turn's error: that error is not its own.
    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let second = claim_pending_turn(&directory);
    fs::write(
        &full,
        user_input(0, &marked("request", &first)) + &user_input(1, &marked("again", &second)),
    )
    .unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 1);

    // Its receipt and turn start arrive: still running, nothing to record.
    let running = created + FORWARDED + QUOTA + &pasted(&second) + FORWARDED;
    fs::write(&log, &running).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 1);
    assert!(directory.join(TURN_CLAIM_FILE).exists());

    // A turn typed by hand after it fails: the newest turn is not the pending one.
    let typed = receipt_line(&go_quoted("typed by hand"));
    fs::write(&log, running.clone() + &typed + FORWARDED + QUOTA).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 1);

    // Agy gives up on the pasted follow-up itself: bound by its receipt.
    let failed = running + QUOTA;
    fs::write(&log, &failed).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 2);
    let error = quota_failure(&read_json(paths.last().unwrap()).unwrap());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());

    // A result Agy writes after the failure was recorded is not accepted: the
    // failed request released its claim.
    let transcript = logs.join("transcript.jsonl");
    fs::write(
        &transcript,
        planner_line(2, &marked("late", &second)) + "\n",
    )
    .unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 2);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.error.as_deref(), Some(error.as_str()));

    // A result that is already written when the error is seen is recorded first.
    update_status(&directory, SessionState::Claimed, None, None).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let third = claim_pending_turn(&directory);
    let mut appended = OpenOptions::new().append(true).open(&transcript).unwrap();
    writeln!(appended, "{}", planner_line(3, &marked("done", &third))).unwrap();
    fs::write(&log, failed + &pasted(&third) + FORWARDED + QUOTA).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    monitor.poll(&directory, &log, &brain).unwrap();
    let paths = event_paths(&directory).unwrap();
    assert_eq!(paths.len(), 3);
    let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
    assert_eq!((latest.message.as_str(), latest.error), ("done", None));
}

#[cfg(windows)]
#[test]
fn windows_brain_root_falls_back_to_userprofile_without_home() {
    assert_eq!(
        default_brain_root(None, Some(std::ffi::OsStr::new(r"C:\Users\agy-user"))).unwrap(),
        PathBuf::from(r"C:\Users\agy-user\.gemini\antigravity-cli\brain")
    );
}
