use super::super::{ADAPTER, correlated_prompt, install_pending_turn, test_support};
use super::*;
use crate::native::provider::NativeProviderAdapter;
use crate::native::session::{SessionState, update_status};

const THREAD: &str = "01234567-89ab-cdef-0123-456789abcdef";
fn fixture() -> (tempfile::TempDir, PendingCodexTurn, Value) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("events")).unwrap();
    update_status(root.path(), SessionState::Working, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(root.path()), &[]).unwrap();
    let pending = install_pending_turn(root.path(), claim.token()).unwrap();
    claim.retain();
    // Mac R0 run E fields, including model/permission_mode unknown to Bridge.
    let payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":THREAD,
        "turn_id":"turn-1", "transcript_path":"rollout.jsonl", "cwd":"workspace",
        "model":"model", "permission_mode":"default",
        "prompt":correlated_prompt(&crate::native::native_delegation_prompt("external", "한글😀"), &pending)});
    (root, pending, payload)
}
fn record(directory: &Path, prefix: &str, token: &str) -> Option<Value> {
    Reader::open_unchecked(directory)
        .private(&record_name(prefix, token).unwrap())
        .optional_json()
        .unwrap()
}
fn notify(payload: &Value) -> Value {
    json!({"type":"agent-turn-complete", "thread-id":payload["session_id"], "turn-id":payload["turn_id"],
        "input-messages":[payload["prompt"]], "last-assistant-message":"result"})
}
fn stop_payload() -> Value {
    json!({"hook_event_name":"Stop", "session_id":THREAD, "turn_id":"turn-1",
        "last_assistant_message":"candidate", "stop_hook_active":false})
}

#[test]
fn submit_rejects_unframed_unmarked_null_path_invalid_thread_and_established_other_thread() {
    for (key, value) in [
        ("prompt", json!("steered")),
        (
            "prompt",
            json!(crate::native::native_delegation_prompt(
                "external",
                "no marker"
            )),
        ),
        ("transcript_path", Value::Null),
        ("session_id", json!("invalid")),
        ("turn_id", Value::Null),
    ] {
        let (root, pending, mut payload) = fixture();
        payload[key] = value;
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        assert!(record(root.path(), BINDING_PREFIX, &pending.claim_token).is_none());
    }
    let (unframed, unframed_pending, mut unframed_payload) = fixture();
    unframed_payload["prompt"] = json!(format!("steered {}", unframed_pending.marker));
    ADAPTER
        .handle_hook(unframed.path(), &unframed_payload)
        .unwrap();
    assert!(
        record(
            unframed.path(),
            BINDING_PREFIX,
            &unframed_pending.claim_token
        )
        .is_none()
    );
    let (root, pending, payload) = fixture();
    test_support::write_established_thread(root.path(), "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    assert!(record(root.path(), BINDING_PREFIX, &pending.claim_token).is_none());
}

#[test]
fn submit_accepts_exact_ide_wrapper_and_identical_rebinding_is_byte_idempotent() {
    let (root, pending, mut payload) = fixture();
    payload["prompt"] = json!(format!(
        "# Context from my IDE setup:\ncontext\n## My request for Codex:\n{}\n ",
        payload["prompt"].as_str().unwrap()
    ));
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    let path = root
        .path()
        .join(record_name(BINDING_PREFIX, &pending.claim_token).unwrap());
    let mut binding = read_binding(root.path(), &pending.claim_token)
        .unwrap()
        .unwrap();
    binding.bound_unix_ms = 1; // Fixed time makes accidental rewriting observable without sleeps.
    Store::open_unchecked(root.path())
        .private(&record_name(BINDING_PREFIX, &pending.claim_token).unwrap())
        .write_json(&binding)
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let binding = read_binding(root.path(), &pending.claim_token)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            binding.thread_id.as_str(),
            binding.turn_id.as_str(),
            binding.transcript_path.as_str()
        ),
        (THREAD, "turn-1", "rollout.jsonl")
    );
}

#[test]
fn rebinding_conflict_is_diagnostic_and_steering_cannot_change_binding() {
    for field in ["turn_id", "transcript_path", "session_id"] {
        let (root, pending, mut payload) = fixture();
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        let before = record(root.path(), BINDING_PREFIX, &pending.claim_token);
        let status = std::fs::read(root.path().join("status.json")).unwrap();
        payload[field] = json!(if field == "session_id" {
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        } else {
            "other"
        });
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        assert_eq!(
            record(root.path(), BINDING_PREFIX, &pending.claim_token),
            before
        );
        assert_eq!(
            record(root.path(), DIAGNOSTIC_PREFIX, &pending.claim_token).unwrap()["reason"],
            "binding_conflict"
        );
        assert_eq!(
            std::fs::read(root.path().join("status.json")).unwrap(),
            status
        );
        payload["prompt"] = json!(format!("steered {}", pending.marker));
        let diagnostic = record(root.path(), DIAGNOSTIC_PREFIX, &pending.claim_token);
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        assert_eq!(
            record(root.path(), DIAGNOSTIC_PREFIX, &pending.claim_token),
            diagnostic
        );
        assert_eq!(
            record(root.path(), BINDING_PREFIX, &pending.claim_token),
            before
        );
        assert!(
            Reader::open_unchecked(root.path())
                .events()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn binding_publication_rechecks_live_claim_after_barrier_ordered_release() {
    use std::sync::{Arc, Barrier};
    let (root, pending, payload) = fixture();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = barrier.clone();
    let directory = root.path().to_path_buf();
    let token = pending.claim_token.clone();
    let worker = std::thread::spawn(move || {
        // Even a previously read live Claim is not authority for a later write.
        assert_eq!(
            turn::current_claim_token(&Reader::open_unchecked(&directory))
                .unwrap()
                .as_deref(),
            Some(pending.claim_token.as_str())
        );
        worker_barrier.wait();
        worker_barrier.wait();
        publish_binding(&directory, &pending, &payload).unwrap();
    });
    barrier.wait();
    let store = Store::open_unchecked(root.path());
    {
        let _lock = store.lock().unwrap();
        store
            .record(crate::native::session::CoreRecord::TurnClaim)
            .remove()
            .unwrap();
    }
    barrier.wait();
    worker.join().unwrap();
    assert!(record(root.path(), BINDING_PREFIX, &token).is_none());
}

#[test]
fn binding_reader_rejects_filename_token_json_token_and_schema_mismatch() {
    let (root, pending, payload) = fixture();
    assert!(read_binding(root.path(), "../escape").is_err());
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    let original = record(root.path(), BINDING_PREFIX, &pending.claim_token).unwrap();
    for (key, value) in [("schema", json!(2)), ("claim_token", json!("1-2-3"))] {
        let mut bad = original.clone();
        bad[key] = value;
        Store::open_unchecked(root.path())
            .private(&record_name(BINDING_PREFIX, &pending.claim_token).unwrap())
            .write_json(&bad)
            .unwrap();
        assert!(read_binding(root.path(), &pending.claim_token).is_err());
    }
}

#[test]
fn stop_overwrites_only_the_current_bound_identity_and_never_publishes() {
    let (root, pending, payload) = fixture();
    let mut stop = stop_payload();
    ADAPTER.handle_hook(root.path(), &stop).unwrap();
    assert!(record(root.path(), CANDIDATE_PREFIX, &pending.claim_token).is_none());
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    ADAPTER.handle_hook(root.path(), &stop).unwrap();
    assert_eq!(
        record(root.path(), CANDIDATE_PREFIX, &pending.claim_token).unwrap()["last_assistant_message"],
        "candidate"
    );
    stop["last_assistant_message"] = Value::Null;
    stop["stop_hook_active"] = json!(true);
    ADAPTER.handle_hook(root.path(), &stop).unwrap();
    let latest = record(root.path(), CANDIDATE_PREFIX, &pending.claim_token).unwrap();
    assert!(latest["last_assistant_message"].is_null());
    assert_eq!(latest["stop_hook_active"], true);
    for key in ["session_id", "turn_id"] {
        let mut wrong = stop.clone();
        wrong[key] = json!("wrong");
        ADAPTER.handle_hook(root.path(), &wrong).unwrap();
        assert_eq!(
            record(root.path(), CANDIDATE_PREFIX, &pending.claim_token).unwrap(),
            latest
        );
    }
    assert!(
        Reader::open_unchecked(root.path())
            .events()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(root.path())).unwrap(),
        Some(pending.claim_token)
    );
}

#[test]
fn bound_notify_is_evidence_only_and_unbound_notify_retains_provenance() {
    for bound in [true, false] {
        let (root, pending, payload) = fixture();
        if bound {
            ADAPTER.handle_hook(root.path(), &payload).unwrap();
        }
        let mut notice = notify(&payload);
        ADAPTER.handle_hook(root.path(), &notice).unwrap();
        let events = Reader::open_unchecked(root.path()).events().unwrap();
        if bound {
            assert!(events.is_empty());
            assert_eq!(
                record(root.path(), NOTIFY_PREFIX, &pending.claim_token).unwrap()["input_messages_matched"],
                true
            );
            notice["input-messages"] = json!(["steered"]);
            ADAPTER.handle_hook(root.path(), &notice).unwrap();
            assert_eq!(
                record(root.path(), NOTIFY_PREFIX, &pending.claim_token).unwrap()["input_messages_matched"],
                false
            );
            assert!(
                turn::current_claim_token(&Reader::open_unchecked(root.path()))
                    .unwrap()
                    .is_some()
            );
        } else {
            let event: crate::native::session::schema::SessionEvent =
                crate::native::session::RecordReader::at(&events[0])
                    .json()
                    .unwrap();
            assert_eq!(event.provenance.as_deref(), Some("codex_notify"));
            assert_eq!(event.message, "result");
            assert!(
                turn::current_claim_token(&Reader::open_unchecked(root.path()))
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn late_notify_after_next_claim_and_unknown_hook_are_no_ops() {
    let (root, pending, payload) = fixture();
    ADAPTER.handle_hook(root.path(), &notify(&payload)).unwrap();
    let claim = turn::claim(&Store::open_unchecked(root.path()), &[]).unwrap();
    let next = install_pending_turn(root.path(), claim.token()).unwrap();
    ADAPTER.handle_hook(root.path(), &notify(&payload)).unwrap();
    ADAPTER
        .handle_hook(root.path(), &json!({"hook_event_name":"FutureEvent"}))
        .unwrap();
    assert_eq!(
        Reader::open_unchecked(root.path()).events().unwrap().len(),
        1
    );
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(root.path())).unwrap(),
        Some(next.claim_token.clone())
    );
    assert!(record(root.path(), NOTIFY_PREFIX, &next.claim_token).is_none());
    assert!(record(root.path(), BINDING_PREFIX, &pending.claim_token).is_none());
}

#[test]
fn submit_without_pending_or_with_ended_session_writes_no_binding() {
    for remove_pending in [true, false] {
        let (root, pending, payload) = fixture();
        if remove_pending {
            Store::open_unchecked(root.path())
                .private(super::super::PENDING_TURN_FILE)
                .remove()
                .unwrap();
        } else {
            update_status(root.path(), SessionState::Exited, Some(0), None).unwrap();
        }
        ADAPTER.handle_hook(root.path(), &payload).unwrap();
        assert!(record(root.path(), BINDING_PREFIX, &pending.claim_token).is_none());
    }
}

#[test]
fn established_thread_accepts_binding_and_stale_stop_does_not_touch_next_claim() {
    let (root, pending, payload) = fixture();
    test_support::write_established_thread(root.path(), THREAD);
    ADAPTER.handle_hook(root.path(), &payload).unwrap();
    assert!(record(root.path(), BINDING_PREFIX, &pending.claim_token).is_some());
    let store = Store::open_unchecked(root.path());
    {
        let _lock = store.lock().unwrap();
        store
            .record(crate::native::session::CoreRecord::TurnClaim)
            .remove()
            .unwrap();
    }
    ADAPTER.handle_hook(root.path(), &stop_payload()).unwrap();
    assert!(record(root.path(), CANDIDATE_PREFIX, &pending.claim_token).is_none());
    let next = turn::claim(&store, &[]).unwrap();
    install_pending_turn(root.path(), next.token()).unwrap();
    ADAPTER.handle_hook(root.path(), &stop_payload()).unwrap();
    assert!(record(root.path(), CANDIDATE_PREFIX, next.token()).is_none());
}
