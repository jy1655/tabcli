use super::super::reduce::{ELIDE_BYTES, Reducer};
use super::*;

const TOOL_HEAD: &str = r#"{"timestamp":"2026-10-23T00:00:00.000Z","ordinal":3,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-large","output":""#;
const TOOL_TAIL: &str = r#""}}"#;

/// `head` + filler + `tail` + LF, one record of `LINE_BYTES + extra` bytes. Built as raw
/// bytes so that fixtures past `LINE_BYTES` stay cheap.
fn huge(head: &str, extra: usize, tail: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(LINE_BYTES + extra + 1);
    bytes.extend_from_slice(head.as_bytes());
    bytes.resize(LINE_BYTES + extra - tail.len(), b'A');
    bytes.extend_from_slice(tail.as_bytes());
    bytes.push(b'\n');
    bytes
}
fn retained(observer: &Observer) -> usize {
    observer.cursor.as_ref().map_or(0, |cursor| {
        cursor.partial.len() + cursor.overflow.as_ref().map_or(0, Reducer::retained)
    })
}
fn assert_suspended(root: &Path, pending: &PendingCodexTurn, reason: &str) {
    assert_pending(root, pending);
    assert_eq!(
        diagnostic(root, pending)["reason"],
        format!("observation_unavailable: {reason}")
    );
}

#[test]
fn image_tool_output_settles_within_the_poll_budget_and_replays_for_the_next_claim() {
    let (root, _, mut payload, mut observer) = bound();
    let path = root.path().join("rollout.jsonl");
    // Shaped like the rollout records of #269: several base64 images, none alone
    // dominant, and a top-level `metadata` after `payload`.
    let image = "A".repeat(LINE_BYTES / 3 + CHUNK_BYTES);
    let mut record = r#"{"timestamp":"2026-10-23T00:00:00.000Z","ordinal":5,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-images","output":[{"type":"input_text","text":"three screenshots"}"#.to_owned();
    for _ in 0..3 {
        record +=
            &format!(r#",{{"type":"input_image","image_url":"data:image/png;base64,{image}"}}"#);
    }
    record += r#"]},"metadata":{"turn_id":"turn-1","sandbox":"read-only"}}"#;
    assert!(record.len() > LINE_BYTES);
    append(&path, record.as_bytes());
    append(&path, b"\n");
    append(&path, &complete(json!("answer after images")));
    let mut polls = 0;
    while events(root.path()).is_empty() {
        let before = observer.cursor.as_ref().map_or(0, |cursor| cursor.offset);
        observer.poll(root.path(), false);
        if let Some(cursor) = &observer.cursor {
            assert!(cursor.suspended.is_none());
            assert!(cursor.offset - before <= CHUNK_BYTES as u64);
        }
        assert!(retained(&observer) <= LINE_BYTES + ELIDE_BYTES);
        polls += 1;
        assert!(polls <= 60, "no settlement after {polls} polls");
    }
    assert!(polls > record.len() / CHUNK_BYTES);
    assert_eq!(events(root.path())[0]["message"], "answer after images");
    // The next claim replays from byte zero across the same record; an interruption
    // still settles as one.
    next_claim(root.path(), &mut payload);
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    append(&path, &prefix(&payload));
    append(
        &path,
        &line(
            "event_msg",
            json!({"type":"turn_aborted","turn_id":payload["turn_id"],"reason":"interrupted"}),
        ),
    );
    observer.poll(root.path(), true);
    let result = events(root.path());
    assert_eq!(result.len(), 2);
    let aborted = result.iter().find(|e| e["error"].is_string()).unwrap();
    assert_eq!(aborted["error"], "interrupted");
    assert_eq!(aborted["provenance"], ABORT_SOURCE);
}

#[test]
fn escapes_and_utf8_across_the_switch_over_and_poll_boundaries_are_carried() {
    for drain in [false, true] {
        let (root, _, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        let start = std::fs::metadata(&path).unwrap().len() as usize;
        let head = r#"{"timestamp":"2026-10-23T00:00:00.000Z","ordinal":4,"type":"response_item","payload":{"type":"function_call_output","call_id":"call-one","output":""#;
        let mut record = huge(head, 5 * CHUNK_BYTES, TOOL_TAIL);
        // Positions in the record: `LINE_BYTES` is the first byte past the retained
        // prefix; a file offset that is a multiple of `CHUNK_BYTES` starts a poll.
        let chunk = |n: usize| n * CHUNK_BYTES - start;
        let (switch, later): (&[u8], &[u8]) = if drain {
            ("😀".as_bytes(), br"\uD83D\uDE00")
        } else {
            (br"\uDBFF\uDFFF", "한".as_bytes())
        };
        for (at, bytes) in [
            (chunk(1) - 1, "한".as_bytes()),
            (chunk(2) - 1, br#"\""#.as_slice()),
            (LINE_BYTES - switch.len() / 2, switch),
            (chunk(34) - later.len() / 2, later),
            (chunk(35) - 1, br"\\".as_slice()),
            (chunk(36) - 3, br"\u00e9".as_slice()),
        ] {
            assert!(at > head.len() && at + bytes.len() < record.len() - TOOL_TAIL.len());
            record[at..at + bytes.len()].copy_from_slice(bytes);
        }
        append(&path, &record);
        append(&path, &complete(json!("answer")));
        let total = std::fs::metadata(&path).unwrap().len() as usize;
        let mut polls = 0;
        while events(root.path()).is_empty() {
            observer.poll(root.path(), drain);
            let cursor = observer.cursor.as_ref();
            assert!(cursor.is_none_or(|cursor| cursor.suspended.is_none()));
            polls += 1;
            assert!(polls <= 60);
        }
        assert_eq!(
            polls,
            if drain {
                1
            } else {
                total.div_ceil(CHUNK_BYTES)
            }
        );
        assert_eq!(events(root.path())[0]["message"], "answer");
    }
}

#[test]
fn only_an_intact_unrelated_response_item_after_session_identity_is_skipped() {
    let long = "L".repeat(ELIDE_BYTES + 1);
    let skip = |record: &str, meta_seen: bool| {
        let mut cursor = Cursor::new("claim".into());
        cursor.meta_seen = meta_seen;
        let reduced = Reducer::from_prefix(record.as_bytes().to_vec())?.finish()?;
        cursor.skip_oversized(&reduced)
    };
    for record in [
        format!(
            r#"{{"timestamp":"t","type":"response_item","payload":{{"type":"custom_tool_call_output","output":"{long}"}},"metadata":{{"m":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"function_call_output","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"payload":{{"output":["{long}","{long}"],"type":"reasoning"}},"metadata":1,"type":"response_item"}}"#
        ),
        format!(
            r#"{{"type":"response\u005fitem","payload":{{"\u0074ype":"custom_tool_call_output","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"{}","output":"{long}"}}}}"#,
            "k".repeat(ELIDE_BYTES)
        ),
    ] {
        assert_eq!(skip(&record, true), Ok(()), "{}", &record[..60]);
        assert_eq!(skip(&record, false), Err("oversized"));
    }
    for record in [
        // Records that can carry evidence.
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","last_agent_message":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"agent_message","turn_id":"other","message":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long}"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"m\u0065ssage","content":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{THREAD}","instructions":"{long}"}}}}"#
        ),
        format!(r#"{{"type":"turn_context","payload":{{"turn_id":"turn-1","text":"{long}"}}}}"#),
        format!(
            r#"{{"type":"compacted","payload":{{"type":"custom_tool_call_output","text":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"unknown","payload":{{"type":"custom_tool_call_output","text":"{long}"}}}}"#
        ),
        // Discriminators that are repeated, cut, empty or of another shape.
        format!(
            r#"{{"type":"response_item","type":"event_msg","payload":{{"type":"x","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"x","output":"{long}"}},"type":"response_item"}}"#
        ),
        format!(
            r#"{{"type":"response_item","type":"response_item","payload":{{"type":"x","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message"}},"payload":{{"type":"x","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"custom_tool_call_output","type":"message","output":"{long}"}}}}"#
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","output":"{long}","type":"custom_tool_call_output"}}}}"#
        ),
        format!(r#"{{"type":"{long}","payload":{{"type":"x"}}}}"#),
        format!(r#"{{"type":"response_item","payload":{{"type":"{long}"}}}}"#),
        format!(r#"{{"type":"response_item","payload":{{"type":"","output":"{long}"}}}}"#),
        format!(r#"{{"type":"response_item","payload":{{"type":7,"output":"{long}"}}}}"#),
        format!(r#"{{"type":"response_item","payload":{{"type":null,"output":"{long}"}}}}"#),
        format!(r#"{{"type":"response_item","payload":{{"output":"{long}"}}}}"#),
        format!(r#"{{"type":"response_item","payload":["{long}"]}}"#),
        format!(r#"{{"type":"response_item","output":"{long}"}}"#),
        format!(r#"{{"type":3,"payload":{{"type":"x","output":"{long}"}}}}"#),
        format!(r#"[{{"type":"response_item","payload":{{"type":"x","output":"{long}"}}}}]"#),
    ] {
        assert_eq!(skip(&record, true), Err("oversized"), "{}", &record[..60]);
    }
    let damaged = format!(r#"{{"type":"response_item","payload":{{"type":"x","output":"{long}"}}"#);
    assert_eq!(skip(&damaged, true), Err("invalid_json"));
}

#[test]
fn oversized_evidence_suspends_and_stop_or_notify_never_settles() {
    let task = r#"{"timestamp":"t","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":""#;
    let user = r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1","content_item_kinds":["user.text"]},"content":[{"type":"input_text","text":""#;
    for (head, tail) in [(task, r#""}}"#), (user, r#""}]}}"#), ("", "")] {
        let (root, pending, payload) = fixture();
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let path = root.path().join("rollout.jsonl");
        if head.is_empty() {
            // The session identity itself.
            let meta = format!(
                r#"{{"timestamp":"t","type":"session_meta","payload":{{"id":"{THREAD}","text":""#
            );
            append(&path, &huge(&meta, 1, TOOL_TAIL));
            append(&path, &prefix(&payload));
        } else {
            append(&path, &prefix(&payload));
            append(&path, &huge(head, 1, tail));
        }
        append(&path, &complete(json!("answer")));
        let mut observer = Observer::default();
        observer.poll(root.path(), true);
        assert_suspended(root.path(), &pending, "oversized");
        assert!(observer.cursor.as_ref().unwrap().overflow.is_none());
        ADAPTER.handle_hook(root.path(), &json!({"hook_event_name":"Stop","session_id":THREAD,"turn_id":"turn-1","last_assistant_message":"candidate","stop_hook_active":false})).unwrap();
        ADAPTER.handle_hook(root.path(), &json!({"type":"agent-turn-complete","thread-id":THREAD,"turn-id":"turn-1","input-messages":[payload["prompt"]],"last-assistant-message":"answer"})).unwrap();
        observer.poll(root.path(), true);
        assert_pending(root.path(), &pending);
    }
}

#[test]
fn faults_in_discarded_text_and_grammar_after_it_suspend_with_their_reason() {
    for (fault, tail, reason) in [
        (b"\xed\xa0\x80".as_slice(), TOOL_TAIL, "invalid_utf8"),
        (b"A".as_slice(), r#""x}}"#, "invalid_json"),
    ] {
        let (root, pending, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        let mut record = huge(TOOL_HEAD, 64, tail);
        let at = LINE_BYTES - fault.len() / 2;
        record[at..at + fault.len()].copy_from_slice(fault);
        append(&path, &record);
        append(&path, &complete(json!("answer")));
        observer.poll(root.path(), true);
        assert_suspended(root.path(), &pending, reason);
    }
}

#[test]
fn reduced_record_past_line_bytes_suspends_before_its_newline() {
    let (root, pending, _, mut observer) = bound();
    let path = root.path().join("rollout.jsonl");
    let start = std::fs::metadata(&path).unwrap().len();
    // Short strings are kept, so the reduced record cannot shrink below the limit.
    let mut record = br#"{"type":"response_item","payload":{"type":"x","output":["#.to_vec();
    while record.len() <= LINE_BYTES + CHUNK_BYTES {
        record.extend_from_slice(br#""a","#);
    }
    append(&path, &record);
    observer.poll(root.path(), true);
    assert_suspended(root.path(), &pending, "oversized");
    assert!(observer.cursor.as_ref().unwrap().offset - start <= LINE_BYTES as u64 + 4);
}

#[test]
fn truncation_and_claim_change_during_overflow() {
    for change in ["truncate", "claim"] {
        let (root, pending, mut payload, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        append(&path, &huge(TOOL_HEAD, 2 * CHUNK_BYTES, TOOL_TAIL));
        while observer
            .cursor
            .as_ref()
            .is_none_or(|c| c.overflow.is_none())
        {
            observer.poll(root.path(), false);
        }
        match change {
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(LINE_BYTES as u64)
                .unwrap(),
            _ => {
                next_claim(root.path(), &mut payload);
                ADAPTER.handle_hook(root.path(), &payload).unwrap();
            }
        }
        append(&path, &complete(json!("answer")));
        if change == "claim" {
            observer.poll(root.path(), false);
            let cursor = observer.cursor.as_ref().unwrap();
            assert!(cursor.overflow.is_none());
            assert_eq!(cursor.offset, CHUNK_BYTES as u64);
            append(&path, &prefix(&payload));
            append(&path, &terminal_for(&payload, json!("next claim")));
            observer.poll(root.path(), true);
            assert_eq!(events(root.path())[0]["message"], "next claim");
            continue;
        }
        observer.poll(root.path(), false);
        assert_suspended(root.path(), &pending, "truncated");
        assert!(observer.cursor.as_ref().unwrap().overflow.is_none());
        observer.poll(root.path(), true);
        assert_suspended(root.path(), &pending, "truncated");
    }
}
