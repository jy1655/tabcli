use super::super::{ADAPTER, PendingCodexTurn, correlated_prompt, install_pending_turn};
use super::*;
use crate::native::provider::{CompletionWatcher, NativeProviderAdapter};
use crate::native::session::{Reader, SessionState, Store, turn, update_status};
use serde_json::{Value, json};
use std::{io::Write, path::Path};
mod questions;
const THREAD: &str = "01234567-89ab-cdef-0123-456789abcdef";
fn fixture() -> (tempfile::TempDir, PendingCodexTurn, Value) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("events")).unwrap();
    update_status(root.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(root.path()), &[]).unwrap();
    let pending = install_pending_turn(root.path(), claim.token()).unwrap();
    claim.retain();
    let payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":THREAD,
        "turn_id":"turn-1", "transcript_path":root.path().join("rollout.jsonl"),
        "prompt":correlated_prompt(&crate::native::native_delegation_prompt("external", "test"), &pending)});
    (root, pending, payload)
}
fn line(kind: &str, payload: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(
        &json!({"timestamp":"2026-10-18T00:00:00.000Z", "ordinal":0,"type":kind,"payload":payload}),
    )
    .unwrap();
    bytes.push(b'\n');
    bytes
}
fn prefix(payload: &Value) -> Vec<u8> {
    let mut bytes = line("session_meta", json!({"id":THREAD}));
    bytes.extend(line("response_item", json!({"type":"message", "role":"user", "content":[{"type":"input_text","text":payload["prompt"]}], "internal_chat_message_metadata_passthrough":{"turn_id":payload["turn_id"],"create_time":1792280000.0,"content_item_kinds":["user.text"]}})));
    bytes
}
fn complete(message: Value) -> Vec<u8> {
    line(
        "event_msg",
        json!({"type":"task_complete","turn_id":"turn-1","root_turn_id":"turn-1","last_agent_message":message,"started_at":1792280000,"completed_at":1792280001,"duration_ms":1000,"time_to_first_token_ms":50}),
    )
}
fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
fn events(path: &Path) -> Vec<Value> {
    Reader::open_unchecked(path)
        .events()
        .unwrap()
        .iter()
        .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
        .collect()
}
#[test]
fn provider_exit_drains_terminal_appended_after_start_before_binding() {
    let (root, _, payload) = fixture();
    let watcher = Box::new(Watcher).start(root.path()).unwrap();
    let path = root.path().join("rollout.jsonl");
    append(&path, &prefix(&payload));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    stop_after_terminal_append(watcher, &path, "answer");
    assert_eq!(events(root.path()).len(), 1);
    assert_eq!(events(root.path())[0]["message"], "answer");
}
fn bound() -> (tempfile::TempDir, PendingCodexTurn, Value, Observer) {
    let (root, pending, payload) = fixture();
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    append(&root.path().join("rollout.jsonl"), &prefix(&payload));
    (root, pending, payload, Observer::default())
}
fn diagnostic(root: &Path, pending: &PendingCodexTurn) -> Value {
    Reader::open_unchecked(root)
        .private(&format!("codex-observer-{}.json", pending.claim_token))
        .optional_json()
        .unwrap()
        .unwrap()
}
fn assert_pending(root: &Path, pending: &PendingCodexTurn) {
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(root))
            .unwrap()
            .as_deref(),
        Some(pending.claim_token.as_str())
    );
    assert!(events(root).is_empty());
}
fn next_claim(root: &Path, payload: &mut Value) -> PendingCodexTurn {
    use crate::native::session::CoreRecord;
    if turn::current_claim_token(&Reader::open_unchecked(root))
        .unwrap()
        .is_some()
    {
        Store::open_unchecked(root)
            .record(CoreRecord::TurnClaim)
            .remove_raw()
            .unwrap();
    }
    if Reader::open_unchecked(root).status().unwrap().state == SessionState::Ready {
        update_status(root, SessionState::Claimed, None, None).unwrap();
        update_status(root, SessionState::Working, None, None).unwrap();
    }
    let claim = turn::claim(&Store::open_unchecked(root), &[]).unwrap();
    let pending = install_pending_turn(root, claim.token()).unwrap();
    claim.retain();
    payload["turn_id"] = json!(format!("turn-{}", pending.claim_token));
    payload["prompt"] = json!(correlated_prompt(
        &crate::native::native_delegation_prompt("external", "next"),
        &pending
    ));
    pending
}
fn terminal_for(payload: &Value, message: Value) -> Vec<u8> {
    line(
        "event_msg",
        json!({"type":"task_complete","turn_id":payload["turn_id"],"last_agent_message":message,"error":null}),
    )
}
#[test]
fn terminal_split_at_every_byte_boundary_including_korean_and_emoji() {
    let bytes = complete(json!("한국어😀 answer"));
    for split in 0..=bytes.len() {
        let (root, pending, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        append(&path, &bytes[..split]);
        observer.poll(root.path(), false);
        if split != bytes.len() {
            assert_pending(root.path(), &pending);
        }
        append(&path, &bytes[split..]);
        observer.poll(root.path(), false);
        assert_eq!(events(root.path()).len(), 1, "split {split}");
        assert_eq!(events(root.path())[0]["message"], "한국어😀 answer");
    }
}
#[test]
fn input_utf8_split_and_no_trailing_newline_wait_without_diagnostic_churn() {
    let (root, pending, mut payload) = fixture();
    payload["prompt"] = json!(correlated_prompt(
        &crate::native::native_delegation_prompt("external", "한😀"),
        &pending
    ));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    let bytes = prefix(&payload);
    let split = bytes.windows(4).position(|w| w == "😀".as_bytes()).unwrap() + 2;
    let path = root.path().join("rollout.jsonl");
    append(&path, &bytes[..split]);
    let mut observer = Observer::default();
    observer.poll(root.path(), false);
    let before = diagnostic(root.path(), &pending);
    observer.poll(root.path(), false);
    assert_eq!(diagnostic(root.path(), &pending), before);
    append(&path, &bytes[split..]);
    let end = complete(json!("ok"));
    append(&path, &end[..end.len() - 1]);
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    append(&path, b"\n");
    observer.poll(root.path(), false);
    assert_eq!(events(root.path())[0]["message"], "ok");
}
#[test]
fn invalid_prefix_utf8_json_and_schema_suspend_without_rejection() {
    for (bad, reason) in [
        (vec![0xff, b'\n'], "invalid_utf8"),
        (b"{broken}\n".to_vec(), "invalid_json"),
        (b"{}\n".to_vec(), "invalid_schema"),
    ] {
        let (root, pending, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        append(&path, &bad);
        append(&path, &complete(json!("answer")));
        observer.poll(root.path(), true);
        assert_pending(root.path(), &pending);
        assert_eq!(
            diagnostic(root.path(), &pending)["reason"],
            format!("observation_unavailable: {reason}")
        );
    }
}
#[test]
fn oversized_unrelated_relevant_and_unterminated_lines_suspend() {
    for kind in ["unknown", "event_msg", "partial"] {
        let (root, pending, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        let huge = "x".repeat(LINE_BYTES + 1);
        let bytes = match kind {
            "event_msg" => complete(json!(huge)),
            "partial" => huge.into_bytes(),
            _ => line(kind, json!({"text":huge})),
        };
        append(&path, &bytes);
        append(&path, &complete(json!("answer")));
        observer.poll(root.path(), false);
        assert!(observer.cursor.as_ref().unwrap().offset <= CHUNK_BYTES as u64);
        observer.poll(root.path(), true);
        assert_pending(root.path(), &pending);
        assert_eq!(
            diagnostic(root.path(), &pending)["reason"],
            "observation_unavailable: oversized"
        );
    }
}
#[test]
fn complete_prefix_requires_one_user_text_with_matching_turn_and_framing() {
    for variant in [
        "wrong_turn",
        "environment",
        "split_content",
        "nested_compaction",
        "no_header",
        "no_marker",
        "metadata_absent",
        "metadata_null",
        "turn_id_absent",
        "unknown_content_type",
        "other_role",
        "other_payload_type",
    ] {
        let (root, pending, payload) = fixture();
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let mut input = json!({"type":"message","role":"user","content":[{"type":"input_text","text":payload["prompt"]}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn-1","content_item_kinds":["user.text"]}});
        match variant {
            "wrong_turn" => {
                input["internal_chat_message_metadata_passthrough"]["turn_id"] = json!("other")
            }
            "environment" => {
                input["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                    json!(["agents_md.instructions", "environments.environment_context"])
            }
            "split_content" => {
                input["content"] = json!([{"type":"input_text","text":crate::native::NATIVE_DELEGATION_HEADER},{"type":"input_text","text":pending.marker}])
            }
            "no_header" => {
                input["content"][0]["text"] = json!(format!("quoted {}", pending.marker))
            }
            "no_marker" => {
                input["content"][0]["text"] = json!(crate::native::NATIVE_DELEGATION_HEADER)
            }
            "metadata_absent" => {
                input
                    .as_object_mut()
                    .unwrap()
                    .remove("internal_chat_message_metadata_passthrough");
            }
            "metadata_null" => input["internal_chat_message_metadata_passthrough"] = Value::Null,
            "turn_id_absent" => {
                input["internal_chat_message_metadata_passthrough"]
                    .as_object_mut()
                    .unwrap()
                    .remove("turn_id");
            }
            "unknown_content_type" => input["content"][0]["type"] = json!("unknown"),
            "other_role" => input["role"] = json!("assistant"),
            "other_payload_type" => input["type"] = json!("function_call"),
            _ => {}
        }
        let path = root.path().join("rollout.jsonl");
        append(&path, &line("session_meta", json!({"id":THREAD})));
        append(
            &path,
            &line(
                if variant == "nested_compaction" {
                    "compacted"
                } else {
                    "response_item"
                },
                input,
            ),
        );
        append(&path, &complete(json!("answer")));
        Observer::default().poll(root.path(), true);
        let result = events(root.path());
        assert_eq!(result.len(), 1, "{variant}");
        assert_eq!(
            result[0]["error"],
            "Codex did not accept the delegated prompt"
        );
        assert_eq!(result[0]["provider_session_id"], THREAD);
        assert_eq!(result[0]["turn_id"], "turn-1");
        assert_eq!(result[0]["provenance"], COMPLETE_SOURCE);
    }
}
#[test]
fn compaction_other_turns_standalone_errors_and_duplicate_terminals_do_not_change_result() {
    let (root, pending, payload, mut observer) = bound();
    let path = root.path().join("rollout.jsonl");
    // Compaction before the accepted input is also just an unrelated record.
    let original = std::fs::read(&path).unwrap();
    let boundary = original.iter().position(|b| *b == b'\n').unwrap() + 1;
    let mut rewritten = original[..boundary].to_vec();
    rewritten.extend(line("compacted", json!({"message":"pre-input"})));
    rewritten.extend_from_slice(&original[boundary..]);
    std::fs::write(&path, rewritten).unwrap();
    append(
        &path,
        &line(
            "compacted",
            json!({"replacement_history": [{"text":"ignored"}]}),
        ),
    );
    let mut other = payload.clone();
    other["turn_id"] = json!("other");
    append(&path, &prefix(&other));
    append(&path, &line("turn_context", json!({"turn_id":"turn-1"})));
    append(
        &path,
        &line(
            "event_msg",
            json!({"type":"error","message":"not terminal"}),
        ),
    );
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    append(
        &path,
        &complete(json!(format!("final\n{}", pending.marker))),
    );
    append(&path, &line("compacted", json!({})));
    append(&path, &complete(json!("duplicate")));
    observer.poll(root.path(), false);
    observer.poll(root.path(), false);
    assert_eq!(events(root.path()).len(), 1);
    assert_eq!(events(root.path())[0]["message"], "final");
    assert_eq!(events(root.path())[0]["provenance"], COMPLETE_SOURCE);
}
#[test]
fn bound_notify_before_abort_never_publishes_success_and_preserves_reason() {
    let (root, pending, payload, mut observer) = bound();
    ADAPTER.handle_hook(root.path(), &json!({"type":"agent-turn-complete", "thread-id":THREAD,"turn-id":"turn-1","input-messages":[payload["prompt"]],"last-assistant-message":"draft"})).unwrap();
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    let path = root.path().join("rollout.jsonl");
    for id in [Value::Null, json!("other"), json!("")] {
        append(
            &path,
            &line(
                "event_msg",
                json!({"type":"turn_aborted","turn_id":id,"reason":"interrupted"}),
            ),
        );
    }
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    append(
        &path,
        &line(
            "event_msg",
            json!({"type":"turn_aborted","turn_id":"turn-1","reason":"budget_limited"}),
        ),
    );
    observer.poll(root.path(), false);
    assert_eq!(events(root.path()).len(), 1);
    assert_eq!(events(root.path())[0]["error"], "budget_limited");
    assert_eq!(events(root.path())[0]["provenance"], ABORT_SOURCE);
}
#[test]
fn terminal_null_blank_marker_only_and_error_never_use_stop_candidate() {
    for message in [Value::Null, json!("   "), json!("marker"), json!("error")] {
        let (root, pending, _, mut observer) = bound();
        ADAPTER.handle_hook(root.path(), &json!({"hook_event_name":"Stop","session_id":THREAD,"turn_id":"turn-1","last_assistant_message":"candidate","stop_hook_active":false})).unwrap();
        let has_error = message == "error";
        let terminal = line(
            "event_msg",
            json!({"type":"task_complete","turn_id":"turn-1","last_agent_message": if message == "marker" { json!(pending.marker) } else {message},"error":if has_error {json!({"message":"terminal error"})} else {Value::Null}}),
        );
        append(&root.path().join("rollout.jsonl"), &terminal);
        observer.poll(root.path(), true);
        let result = events(root.path());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["message"], "");
        assert_eq!(result[0]["provenance"], COMPLETE_SOURCE);
        assert_eq!(
            result[0]["error"],
            if has_error {
                "{\"message\":\"terminal error\"}"
            } else {
                "Codex turn ended without an assistant message"
            }
        );
    }
}
#[test]
fn missing_wrong_typed_answer_and_wrong_session_meta_suspend() {
    for variant in ["missing", "number", "wrong_meta"] {
        let (root, pending, payload) = fixture();
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let path = root.path().join("rollout.jsonl");
        if variant == "wrong_meta" {
            append(&path, &line("session_meta", json!({"id":"other"})));
        } else {
            append(&path, &prefix(&payload));
        }
        let mut terminal = json!({"type":"task_complete","turn_id":"turn-1","error":null});
        if variant == "number" {
            terminal["last_agent_message"] = json!(5);
        }
        append(&path, &line("event_msg", terminal));
        Observer::default().poll(root.path(), true);
        assert_pending(root.path(), &pending);
        assert_eq!(diagnostic(root.path(), &pending)["state"], "suspended");
    }
}
#[test]
fn truncation_same_size_larger_replacement_and_disappearance_suspend_permanently() {
    for change in ["truncate", "same", "larger", "missing", "directory"] {
        let (root, pending, _, mut observer) = bound();
        let path = root.path().join("rollout.jsonl");
        observer.poll(root.path(), false);
        let original = std::fs::read(&path).unwrap();
        if change == "truncate" {
            std::fs::write(&path, b"").unwrap();
        } else {
            std::fs::rename(&path, root.path().join("old.jsonl")).unwrap();
            match change {
                "same" => append(&path, &original),
                "larger" => {
                    append(&path, &original);
                    append(&path, &complete(json!("wrong")));
                }
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => {}
            }
        }
        observer.poll(root.path(), false);
        assert_pending(root.path(), &pending);
        assert_eq!(
            diagnostic(root.path(), &pending)["reason"],
            format!(
                "observation_unavailable: {}",
                match change {
                    "truncate" => "truncated",
                    "same" | "larger" => "replaced",
                    _ => "read_error",
                }
            )
        );
        if change == "directory" {
            std::fs::remove_dir(&path).unwrap();
        }
        if change == "missing" || change == "directory" {
            append(&path, &original);
        }
        append(&path, &complete(json!("must not recover")));
        let before = diagnostic(root.path(), &pending);
        observer.poll(root.path(), true);
        assert_pending(root.path(), &pending);
        assert_eq!(diagnostic(root.path(), &pending), before);
    }
}
#[test]
fn session_loop_survives_unbound_notify_idle_two_claims_and_large_second_answer() {
    let (root, _, mut payload) = fixture();
    let mut observer = Observer::default();
    for _ in 0..3 {
        observer.poll(root.path(), false);
    }
    ADAPTER.handle_hook(root.path(), &json!({"type":"agent-turn-complete","thread-id":THREAD,"turn-id":"turn-1","input-messages":[payload["prompt"]],"last-assistant-message":"legacy"})).unwrap();
    for _ in 0..3 {
        observer.poll(root.path(), false);
        assert!(observer.cursor.is_none());
    }
    for size in [20, 40000] {
        next_claim(root.path(), &mut payload);
        for _ in 0..2 {
            observer.poll(root.path(), false);
        }
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let path = root.path().join("rollout.jsonl");
        append(&path, &prefix(&payload));
        append(&path, &terminal_for(&payload, json!("x".repeat(size))));
        observer.poll(root.path(), true);
        assert!(observer.cursor.is_none());
        for _ in 0..2 {
            observer.poll(root.path(), false);
        }
    }
    let result = events(root.path());
    assert_eq!(result.len(), 3);
    assert!(
        result
            .iter()
            .any(|e| e["message"].as_str().unwrap().len() == 40000)
    );
    Box::new(Watcher)
        .start(root.path())
        .unwrap()
        .stop()
        .unwrap();
}
#[test]
fn absent_file_suspends_only_that_claim_and_next_healthy_claim_arms() {
    let (root, pending, mut payload) = fixture();
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    let mut observer = Observer::default();
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    let path = root.path().join("rollout.jsonl");
    append(&path, &prefix(&payload));
    append(&path, &complete(json!("late")));
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    next_claim(root.path(), &mut payload);
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    append(&path, &prefix(&payload));
    append(&path, &terminal_for(&payload, json!("healthy")));
    observer.poll(root.path(), true);
    assert_eq!(events(root.path()).len(), 1);
    assert_eq!(events(root.path())[0]["message"], "healthy");
}
#[test]
fn stale_claim_at_barrier_ordered_settlement_publishes_nothing() {
    use std::sync::{Arc, Barrier};
    let (root, _, mut payload, mut observer) = bound();
    observer.poll(root.path(), false);
    append(&root.path().join("rollout.jsonl"), &complete(json!("old")));
    let mut cursor = observer.cursor.take().unwrap();
    let terminal = cursor.read(true).unwrap().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let directory = root.path().to_owned();
    let worker = std::thread::spawn(move || {
        other.wait();
        other.wait();
        assert!(!cursor.settle(&directory, terminal).unwrap());
    });
    barrier.wait();
    let pending = next_claim(root.path(), &mut payload);
    barrier.wait();
    worker.join().unwrap();
    assert_pending(root.path(), &pending);
}
#[test]
fn diagnostic_write_fault_does_not_publish_failure_or_stop_later_claims() {
    let (root, pending, mut payload, mut observer) = bound();
    crate::native::session::primitives::with_fault_budget(0, || observer.poll(root.path(), false));
    assert_pending(root.path(), &pending);
    append(&root.path().join("rollout.jsonl"), b"bad json\n");
    crate::native::session::primitives::with_fault_budget(0, || observer.poll(root.path(), false));
    assert_pending(root.path(), &pending);
    next_claim(root.path(), &mut payload);
    payload["transcript_path"] = json!(root.path().join("healthy.jsonl"));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    append(&root.path().join("healthy.jsonl"), &prefix(&payload));
    append(
        &root.path().join("healthy.jsonl"),
        &terminal_for(&payload, json!("healthy")),
    );
    observer.poll(root.path(), true);
    assert_eq!(events(root.path())[0]["message"], "healthy");
}
#[test]
fn alias_ide_wrapped_input_and_bounded_terminal_error() {
    for error in [false, true] {
        let (root, pending, mut payload) = fixture();
        payload["prompt"] = json!(format!(
            "# Context from my IDE setup:\ncontext\n## My request for Codex:\n{}",
            payload["prompt"].as_str().unwrap()
        ));
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let path = root.path().join("rollout.jsonl");
        append(&path, &prefix(&payload));
        append(
            &path,
            &line(
                "event_msg",
                json!({"type":"turn_complete","turn_id":"turn-1","last_agent_message":"ok","error":if error {json!({"message":"한".repeat(ERROR_CHARS+50)})} else {Value::Null}}),
            ),
        );
        Observer::default().poll(root.path(), true);
        let result = events(root.path());
        assert_eq!(result.len(), 1);
        if error {
            assert_eq!(
                result[0]["error"].as_str().unwrap().chars().count(),
                ERROR_CHARS
            );
        } else {
            assert_eq!(result[0]["message"], "ok");
        }
        assert_eq!(diagnostic(root.path(), &pending)["state"], "settled");
    }
}
#[test]
fn final_drain_crosses_poll_budget_and_keeps_partial_terminal_unpublished() {
    for terminated in [false, true] {
        let (root, pending, payload) = fixture();
        let watcher = Box::new(Watcher).start(root.path()).unwrap();
        let path = root.path().join("rollout.jsonl");
        append(&path, &prefix(&payload));
        for _ in 0..4 {
            append(
                &path,
                &line("unknown", json!({"text":"x".repeat(CHUNK_BYTES)})),
            );
        }
        let terminal = complete(json!("drained"));
        append(
            &path,
            &terminal[..terminal.len() - usize::from(!terminated)],
        );
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        watcher.stop().unwrap();
        if terminated {
            assert_eq!(events(root.path())[0]["message"], "drained");
        } else {
            assert_pending(root.path(), &pending);
        }
    }
}
#[test]
fn malformed_or_changed_binding_suspends_without_opening_another_file() {
    for field in [
        "schema",
        "claim_token",
        "thread_id",
        "turn_id",
        "transcript_path",
    ] {
        let (root, pending, _, mut observer) = bound();
        if field == "transcript_path" {
            observer.poll(root.path(), false);
        }
        let name = format!("codex-turn-{}.json", pending.claim_token);
        let mut binding: Value = Reader::open_unchecked(root.path())
            .private(&name)
            .optional_json()
            .unwrap()
            .unwrap();
        binding[field] = if field == "schema" {
            json!(2)
        } else if field == "turn_id" {
            json!("")
        } else {
            json!("invalid")
        };
        Store::open_unchecked(root.path())
            .private(&name)
            .write_json(&binding)
            .unwrap();
        append(
            &root.path().join("rollout.jsonl"),
            &complete(json!("answer")),
        );
        observer.poll(root.path(), true);
        assert_pending(root.path(), &pending);
        assert_eq!(
            diagnostic(root.path(), &pending)["reason"],
            "observation_unavailable: binding_conflict"
        );
    }
}
#[test]
fn no_claim_discards_armed_cursor_and_late_notify_cannot_settle_next_bound_claim() {
    let (root, _, old, mut observer) = bound();
    observer.poll(root.path(), false);
    Store::open_unchecked(root.path())
        .record(crate::native::session::CoreRecord::TurnClaim)
        .remove_raw()
        .unwrap();
    observer.poll(root.path(), false);
    assert!(observer.cursor.is_none());
    let mut payload = old.clone();
    let pending = next_claim(root.path(), &mut payload);
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    ADAPTER.handle_hook(root.path(), &json!({"type":"agent-turn-complete","thread-id":THREAD,"turn-id":"turn-1","input-messages":[old["prompt"]],"last-assistant-message":"late"})).unwrap();
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    append(&root.path().join("rollout.jsonl"), &prefix(&payload));
    append(
        &root.path().join("rollout.jsonl"),
        &terminal_for(&payload, json!("new")),
    );
    observer.poll(root.path(), true);
    assert_eq!(events(root.path())[0]["message"], "new");
}
#[cfg(windows)]
#[test]
fn windows_exclusive_read_failure_then_reappearance_never_rearms_claim() {
    use std::os::windows::fs::OpenOptionsExt;
    let (root, pending, payload) = fixture();
    let path = root.path().join("rollout.jsonl");
    append(&path, &prefix(&payload));
    append(&path, &complete(json!("answer")));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    let exclusive = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let mut observer = Observer::default();
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    assert_eq!(
        diagnostic(root.path(), &pending)["reason"],
        "observation_unavailable: read_error"
    );
    drop(exclusive);
    observer.poll(root.path(), true);
    assert_pending(root.path(), &pending);
}
#[test]
fn launch_plan_owns_session_watcher_even_before_hook_availability_is_known() {
    let (root, _, payload) = fixture();
    let plan = super::super::prepare_launch_for_platform(
        crate::native::provider::LaunchContext {
            bridge_executable: Path::new("C:/bridge/tabcli.exe"),
            directory: root.path(),
            workspace: root.path(),
            title: "test",
            prompt: "test",
            yolo: false,
            model: None,
            effort: None,
        },
        cfg!(windows),
        |_, _| Ok(false),
    )
    .unwrap();
    let watcher = plan.watcher.start(root.path()).unwrap();
    let path = root.path().join("rollout.jsonl");
    append(&path, &prefix(&payload));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    stop_after_terminal_append(watcher, &path, "from plan");
    assert_eq!(events(root.path())[0]["message"], "from plan");
}

// Thread-local scheduling follows session::primitives::close_io_test. Callbacks
// schedule actual file mutations at I/O boundaries; no sleeps or mocked decisions.
#[derive(Clone, Copy)]
pub(super) enum ReadCheckpoint {
    BeforePostReadVerify,
    AfterPostReadVerify,
    AfterTerminalVerify,
    BeforeQuestionLock,
}
type ReadHook = Box<dyn FnMut(ReadCheckpoint)>;
thread_local! {
    static READ_HOOK: std::cell::RefCell<Option<ReadHook>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn at_read_checkpoint(point: ReadCheckpoint) {
    READ_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(point);
        }
    });
}
fn during_read<T>(hook: impl FnMut(ReadCheckpoint) + 'static, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<ReadHook>);
    impl Drop for Reset {
        fn drop(&mut self) {
            READ_HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(READ_HOOK.with(|slot| slot.replace(Some(Box::new(hook)))));
    run()
}
#[test]
fn matching_turn_malformed_attribution_suspends_without_releasing_claim() {
    for variant in [
        "kinds_object",
        "kinds_number",
        "content_object",
        "content_number",
        "text_number",
        "content_type_number",
        "content_type_missing",
        "role_number",
        "turn_id_number",
        "payload_type_number",
        "metadata_number",
    ] {
        let (root, pending, payload) = fixture();
        let mut input = json!({"type":"message","role":"user",
            "content":[{"type":"input_text","text":payload["prompt"]}],
            "internal_chat_message_metadata_passthrough":{"turn_id":"turn-1","content_item_kinds":["user.text"]}});
        match variant {
            "kinds_object" => {
                input["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                    json!({"damaged":true})
            }
            "kinds_number" => {
                input["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                    json!(["user.text", 3])
            }
            "content_object" => input["content"] = json!({}),
            "content_number" => input["content"] = json!([3]),
            "text_number" => input["content"][0]["text"] = json!(3),
            "content_type_number" => input["content"][0]["type"] = json!(3),
            "content_type_missing" => {
                input["content"][0].as_object_mut().unwrap().remove("type");
            }
            "role_number" => input["role"] = json!(3),
            "turn_id_number" => {
                input["internal_chat_message_metadata_passthrough"]["turn_id"] = json!(3)
            }
            "payload_type_number" => input["type"] = json!(3),
            "metadata_number" => input["internal_chat_message_metadata_passthrough"] = json!(3),
            _ => unreachable!(),
        }
        let path = root.path().join("rollout.jsonl");
        append(&path, &line("session_meta", json!({"id":THREAD})));
        append(&path, &line("response_item", input));
        append(&path, &complete(json!("answer")));
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        Observer::default().poll(root.path(), true);
        assert_pending(root.path(), &pending);
        let diagnostic = diagnostic(root.path(), &pending);
        assert_eq!(diagnostic["state"], "suspended", "{variant}");
        assert_eq!(
            diagnostic["reason"], "observation_unavailable: invalid_schema",
            "{variant}"
        );
    }
}
#[test]
fn unaccepted_aborted_turn_preserves_terminal_provenance() {
    let (root, _, payload) = fixture();
    let path = root.path().join("rollout.jsonl");
    append(&path, &line("session_meta", json!({"id":THREAD})));
    append(
        &path,
        &line(
            "event_msg",
            json!({"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}),
        ),
    );
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    Observer::default().poll(root.path(), true);
    let result = events(root.path());
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0]["error"],
        "Codex did not accept the delegated prompt"
    );
    assert_eq!(result[0]["provenance"], "codex_transcript_turn_aborted");
    assert!(
        turn::current_claim_token(&Reader::open_unchecked(root.path()))
            .unwrap()
            .is_none()
    );
}
#[test]
fn post_read_observed_growth_then_truncation_suspends_with_terminal_still_present() {
    let (root, pending, _, mut observer) = bound();
    observer.poll(root.path(), false);
    let path = root.path().join("rollout.jsonl");
    append(&path, &line("unknown", json!({})));
    let read_limit = std::fs::metadata(&path).unwrap().len();
    let terminal = complete(json!("answer"));
    let retained_length = read_limit + terminal.len() as u64;
    let mutation_path = path.clone();
    during_read(
        move |point| match point {
            ReadCheckpoint::BeforePostReadVerify => {
                append(&mutation_path, &terminal);
                append(
                    &mutation_path,
                    &line("unknown", json!({"tail":"discarded"})),
                );
            }
            ReadCheckpoint::AfterPostReadVerify => {
                assert!(std::fs::metadata(&mutation_path).unwrap().len() > retained_length);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&mutation_path)
                    .unwrap()
                    .set_len(retained_length)
                    .unwrap();
            }
            ReadCheckpoint::BeforeQuestionLock | ReadCheckpoint::AfterTerminalVerify => (),
        },
        || observer.poll(root.path(), false),
    );
    assert_pending(root.path(), &pending);
    assert_eq!(observer.cursor.as_ref().unwrap().offset, read_limit);
    assert!(retained_length > read_limit);
    assert!(
        std::fs::read(&path)
            .unwrap()
            .ends_with(&complete(json!("answer")))
    );
    observer.poll(root.path(), false);
    assert_pending(root.path(), &pending);
    let diagnostic = diagnostic(root.path(), &pending);
    assert_eq!(diagnostic["state"], "suspended");
    assert_eq!(diagnostic["reason"], "observation_unavailable: truncated");
}
#[test]
fn final_drain_growth_updates_high_water_without_extending_eof_snapshot() {
    let (root, pending, _, mut observer) = bound();
    observer.poll(root.path(), false);
    let path = root.path().join("rollout.jsonl");
    append(&path, &line("unknown", json!({})));
    let snapshot = std::fs::metadata(&path).unwrap().len();
    let mutation_path = path.clone();
    during_read(
        move |point| {
            if matches!(point, ReadCheckpoint::BeforePostReadVerify) {
                append(&mutation_path, &complete(json!("after snapshot")));
            }
        },
        || observer.poll(root.path(), true),
    );
    assert_pending(root.path(), &pending);
    let cursor = observer.cursor.as_ref().unwrap();
    assert_eq!(cursor.offset, snapshot);
    assert_eq!(cursor.extent, std::fs::metadata(&path).unwrap().len());
    assert!(cursor.extent > snapshot);
}

// The provider writer acknowledges the flushed append before shutdown/drain.
// The worker may poll at any point: the valid prefix predates the binding.
fn stop_after_terminal_append(watcher: Box<dyn ActiveWatcher>, path: &Path, answer: &str) {
    std::thread::scope(|scope| {
        let (sent, received) = std::sync::mpsc::sync_channel(0);
        let writer = scope.spawn(move || {
            append(path, &complete(json!(answer)));
            sent.send(()).unwrap();
        });
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        watcher.stop().unwrap();
        writer.join().unwrap();
    });
}
