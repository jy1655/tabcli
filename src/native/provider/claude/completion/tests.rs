use super::*;
use crate::native::clock::FakeClock;
use crate::native::session::{CoreRecord, SessionState, schema::SessionEvent};

const CONVERSATION: &str = "00000000-0000-4000-8000-000000000249";
const REQUEST: &str = "claude-turn-native-test";
const TURN: &str = "00000000-0000-4000-8000-000000000250";

struct Fixture {
    root: tempfile::TempDir,
    token: String,
    message: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("events")).unwrap();
        crate::native::session::update_status(root.path(), SessionState::Working, None, None)
            .unwrap();
        let store = Store::open_unchecked(root.path());
        let claim = turn::claim(&store, &[]).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        store
            .private(MODE_FILE)
            .write_json(&LaunchMode {
                schema: 1,
                mode: Mode::NativeV1,
                reason: "test".into(),
                claude_version: Some("2.1.302".into()),
            })
            .unwrap();
        let pending = PendingCrossSessionTurn::new(REQUEST).unwrap();
        let message = super::super::messenger::cross_session_target_message(
            "[Agent Bridge native delegation]\nSource: external\n\n< & \"한😀\r\n</cross-session-message>",
            &pending,
        );
        install_pending(root.path(), REQUEST, Some(&token), &message)
            .unwrap()
            .retain();
        Self {
            root,
            token,
            message,
        }
    }
    fn store(&self) -> Store {
        Store::open_unchecked(self.root.path())
    }
    fn frame(&self) -> String {
        format!(
            "<cross-session-message from=\"uds:\\\\.\\pipe\\LOCAL\\test\" from-name=\"sender\" from-mode=\"prompting\">\n{}\n</cross-session-message>",
            self.message
        )
    }
    fn wire(&self, event: &str, data: Value) -> Value {
        json!({"agent_bridge_claude_mod":1,"event":event,"conversation":CONVERSATION,"data":data})
    }
    fn send(&self, event: &str, data: Value) {
        receive(self.root.path(), &self.wire(event, data)).unwrap();
    }
    fn start(&self) {
        self.send("turn.start", json!({"turnId":TURN,"text":self.frame()}));
    }
    fn complete(&self, answer: &str) {
        self.send(
            "turn.complete",
            json!({"turnId":TURN,"reason":"answer","isAborted":false,"answer":answer}),
        );
    }
    fn events(&self) -> Vec<SessionEvent> {
        self.store()
            .events()
            .unwrap()
            .iter()
            .map(|path| {
                crate::native::session::RecordReader::at(path)
                    .json()
                    .unwrap()
            })
            .collect()
    }
    fn retained(&self) {
        assert!(self.events().is_empty());
        assert_eq!(
            turn::current_claim_token(&self.store()).unwrap().as_deref(),
            Some(self.token.as_str())
        );
    }
}

#[test]
fn queued_start_needs_no_submission_scope_and_duplicate_events_publish_once() {
    let f = Fixture::new();
    f.start();
    f.start();
    super::super::handle_hook(f.root.path(), &json!({"hook_event_name":"Stop","last_assistant_message":format!("draft\n<!-- agent-bridge-claude-turn:{REQUEST} -->")})).unwrap();
    super::super::handle_hook(
        f.root.path(),
        &json!({"hook_event_name":"StopFailure","error":"draft API error"}),
    )
    .unwrap();
    f.retained();
    f.complete("final without marker");
    f.complete("duplicate");
    let events = f.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "final without marker");
    assert_eq!(events[0].turn_id.as_deref(), Some(REQUEST));
}

#[test]
fn only_the_exact_outer_frame_can_bind_even_with_delimiters_in_its_body() {
    let f = Fixture::new();
    let frame = f.frame();
    let variants = [
        f.message.clone(),
        format!("quoted\n{frame}"),
        format!("{frame}\nextra"),
        frame.replace("Source: external", "Source: changed"),
        frame.replace(REQUEST, "claude-turn-other"),
        frame.replace("from-mode=\"prompting\"", "unknown=\"prompting\""),
        format!(
            "<cross-session-message from=\"x\" from-name=\"y\" from-mode=\"prompting\">\n{frame}\n</cross-session-message>"
        ),
        format!("<!-- agent-bridge-claude-turn:{REQUEST} -->"),
    ];
    for text in variants {
        f.send("turn.start", json!({"turnId":TURN,"text":text}));
        f.complete("unrelated");
        f.retained();
    }
    f.start();
    f.complete("accepted");
    assert_eq!(f.events()[0].message, "accepted");
}

#[test]
fn subagents_conflicting_identities_and_other_conversations_cannot_settle() {
    let f = Fixture::new();
    for agent in [json!("child"), json!(""), Value::Null] {
        f.send(
            "turn.start",
            json!({"turnId":TURN,"text":f.frame(),"agentId":agent}),
        );
        f.complete("unbound");
        f.retained();
    }
    f.start();
    f.send(
        "turn.start",
        json!({"turnId":"other-turn","text":f.frame()}),
    );
    f.send(
        "turn.complete",
        json!({"turnId":"other-turn","reason":"answer","isAborted":false,"answer":"wrong"}),
    );
    f.send("turn.complete",json!({"turnId":TURN,"reason":"answer","isAborted":false,"answer":"child","agentId":"child"}));
    let mut other = f.wire(
        "turn.complete",
        json!({"turnId":TURN,"reason":"answer","isAborted":false,"answer":"foreign"}),
    );
    other["conversation"] = json!("00000000-0000-0000-0000-000000000000");
    receive(f.root.path(), &other).unwrap();
    f.retained();
    f.complete("bound");
    assert_eq!(f.events()[0].message, "bound");
}

#[test]
fn replaced_claim_and_closed_session_fence_late_completion() {
    for close in [false, true] {
        let f = Fixture::new();
        f.start();
        if close {
            crate::native::session::update_status(f.root.path(), SessionState::Closed, None, None)
                .unwrap();
        } else {
            std::fs::write(
                f.store().record(CoreRecord::TurnClaim).path(),
                "999-999-999",
            )
            .unwrap();
        }
        f.complete("late");
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
fn old_pending_cleanup_cannot_delete_a_new_request() {
    let f = Fixture::new();
    let old = install_pending(f.root.path(), REQUEST, Some(&f.token), &f.message).unwrap();
    let next = PendingCrossSessionTurn::new("claude-turn-next").unwrap();
    let message = super::super::messenger::cross_session_target_message(
        "[Agent Bridge native delegation]\nSource: external\n\nnext",
        &next,
    );
    install_pending(f.root.path(), &next.request_id, Some(&f.token), &message)
        .unwrap()
        .retain();
    drop(old);
    let pending: PendingCrossSessionTurn = f.store().private(PENDING_TURN_FILE).json().unwrap();
    assert_eq!(pending.request_id, next.request_id);
}

#[test]
fn readiness_is_runtime_evidence_and_never_replaces_the_claim_or_deadline() {
    let f = Fixture::new();
    let now = Instant::now();
    let mut clock = FakeClock::new(now);
    let error = admit(
        f.root.path(),
        &f.token,
        now + Duration::from_millis(150),
        true,
        &mut clock,
    )
    .unwrap_err();
    assert!(error.to_string().contains("readiness was not observed"));
    f.send("session.start", json!({}));
    let mut clock = FakeClock::new(now);
    admit(
        f.root.path(),
        &f.token,
        now + Duration::from_secs(1),
        true,
        &mut clock,
    )
    .unwrap();
    assert!(admit(f.root.path(), &f.token, now, true, &mut clock).is_err());
    assert!(
        admit(
            f.root.path(),
            "999-999-999",
            now + Duration::from_secs(1),
            true,
            &mut clock
        )
        .is_err()
    );
    f.store()
        .private(READY_FILE)
        .write_json(&json!({"schema":1,"revision":2,"conversation":CONVERSATION}))
        .unwrap();
    assert!(
        admit(
            f.root.path(),
            &f.token,
            now + Duration::from_secs(1),
            true,
            &mut clock
        )
        .is_err()
    );
    crate::native::session::update_status(f.root.path(), SessionState::Closed, None, None).unwrap();
    assert!(
        admit(
            f.root.path(),
            &f.token,
            now + Duration::from_secs(1),
            true,
            &mut clock
        )
        .is_err()
    );
}

#[test]
fn legacy_windows_refuses_new_input_while_macos_preserves_its_route() {
    let f = Fixture::new();
    f.store().private(MODE_FILE).remove().unwrap();
    let now = Instant::now();
    let mut clock = FakeClock::new(now);
    assert!(
        admit(
            f.root.path(),
            &f.token,
            now + Duration::from_secs(1),
            true,
            &mut clock
        )
        .unwrap_err()
        .to_string()
        .contains("reopen")
    );
    admit(
        f.root.path(),
        &f.token,
        now + Duration::from_secs(1),
        false,
        &mut clock,
    )
    .unwrap();
    f.store()
        .private(MODE_FILE)
        .write_json(&LaunchMode {
            schema: 1,
            mode: Mode::LegacyStop,
            reason: "platform_legacy".into(),
            claude_version: None,
        })
        .unwrap();
    assert!(
        admit(
            f.root.path(),
            &f.token,
            now + Duration::from_secs(1),
            true,
            &mut clock
        )
        .is_err()
    );
    assert!(settings(Path::new("bridge"), true).get("hooks").is_none());
    assert!(settings(Path::new("bridge"), false)["hooks"]["Stop"].is_array());
}

#[test]
fn answer_markers_are_optional_and_only_the_exact_suffix_is_removed() {
    for (answer, expected) in [
        ("final".to_owned(), "final".to_owned()),
        (
            format!("middle <!-- agent-bridge-claude-turn:{REQUEST} --> end"),
            format!("middle <!-- agent-bridge-claude-turn:{REQUEST} --> end"),
        ),
        (
            format!("final\n<!-- agent-bridge-claude-turn:{REQUEST} -->\n"),
            "final".into(),
        ),
    ] {
        let f = Fixture::new();
        f.start();
        f.complete(&answer);
        assert_eq!(f.events()[0].message, expected);
    }
    for answer in [
        "".to_owned(),
        " \n".into(),
        format!("<!-- agent-bridge-claude-turn:{REQUEST} -->"),
    ] {
        let f = Fixture::new();
        f.start();
        f.complete(&answer);
        assert!(
            f.events()[0]
                .error
                .as_deref()
                .unwrap()
                .contains("no answer text")
        );
    }
}

#[test]
fn provider_terminal_failures_are_explicit_and_malformed_outcomes_retain_authority() {
    for (data, error) in [
        (
            json!({"reason":"aborted","isAborted":true,"answer":"partial"}),
            "interrupted",
        ),
        (
            json!({"reason":"error","isAborted":false,"answer":"rate limited"}),
            "API error: rate limited",
        ),
        (
            json!({"reason":"refusal","isAborted":false,"answer":"","refusal":{"category":"policy","explanation":"declined"}}),
            "policy: declined",
        ),
    ] {
        let f = Fixture::new();
        f.start();
        let mut data = data;
        data["turnId"] = json!(TURN);
        f.send("turn.complete", data);
        let event = f.events().remove(0);
        assert!(event.error.unwrap().contains(error));
        assert!(!event.cancelled);
    }
    for data in [
        json!({"reason":"aborted","isAborted":false,"answer":"x"}),
        json!({"reason":"answer","isAborted":true,"answer":"x"}),
        json!({"reason":"unknown","isAborted":false,"answer":"x"}),
        json!({"reason":"answer","isAborted":false,"answer":null}),
        json!({"reason":"refusal","isAborted":false,"answer":"x","refusal":{"category":1,"explanation":null}}),
        json!({"reason":"answer","isAborted":false,"answer":"x","refusal":null}),
    ] {
        let f = Fixture::new();
        f.start();
        let mut data = data;
        data["turnId"] = json!(TURN);
        f.send("turn.complete", data);
        f.retained();
        assert!(f.store().private(DIAGNOSTIC_FILE).text().unwrap().is_some());
    }
}

#[test]
fn clear_can_change_conversation_without_revoking_module_capability() {
    let f = Fixture::new();
    let mut ready = f.wire("session.start", json!({}));
    ready["conversation"] = json!("00000000-0000-0000-0000-000000000000");
    receive(f.root.path(), &ready).unwrap();
    f.start();
    f.complete("new conversation");
    assert_eq!(f.events()[0].message, "new conversation");
}

#[test]
fn wire_admission_counts_json_escaping_and_oversize_requires_the_bound_identity() {
    let f = Fixture::new();
    let overhead = start_wire_size("").unwrap();
    assert_eq!(
        start_wire_size(&"x".repeat(MAX_WIRE_BYTES - overhead)).unwrap(),
        MAX_WIRE_BYTES
    );
    assert!(
        install_pending(
            f.root.path(),
            REQUEST,
            Some(&f.token),
            &"x".repeat(MAX_WIRE_BYTES - overhead + 1)
        )
        .is_err()
    );
    assert_eq!(
        start_wire_size("\n\"\\한😀").unwrap() - overhead,
        2 + 2 + 2 + 3 + 4
    );
    f.send(
        "oversize",
        json!({"turnId":TURN,"utf8_len":MAX_WIRE_BYTES+1}),
    );
    f.retained();
    f.start();
    f.send("oversize", json!({"turnId":TURN,"utf8_len":MAX_WIRE_BYTES}));
    f.retained();
    f.send(
        "oversize",
        json!({"turnId":TURN,"utf8_len":MAX_WIRE_BYTES+1}),
    );
    assert!(
        f.events()[0]
            .error
            .as_deref()
            .unwrap()
            .contains("not truncated")
    );
}

#[test]
fn windows_selects_native_mode_from_the_recorded_stable_version_only() {
    for version in [
        "2.1.301",
        "2.1.302-beta.1",
        "unknown",
        "2.1.302 (Claude Code)",
        "2.2.0",
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-floor");
        std::fs::create_dir(&directory).unwrap();
        let store = Store::open_unchecked(&directory);
        store
            .write_manifest(&crate::native::session::schema::SessionManifest {
                schema: 1,
                id: "session-floor".into(),
                provider: "claude".into(),
                provider_path: "unused".into(),
                provider_version: version.into(),
                workspace: root.path().into(),
                title: "version fixture".into(),
                model: None,
                effort: None,
                yolo: true,
                created_unix_ms: 1,
            })
            .unwrap();
        let eligible = ["2.1.302 (Claude Code)", "2.2.0"].contains(&version);
        assert_eq!(prepare(&directory, true).is_ok(), eligible, "{version}");
        assert_eq!(native_mode(&directory).unwrap(), eligible);
        if eligible {
            assert!(directory.join("claude-mod/hooks/register.js").is_file());
            assert!(prepare(&directory, false).is_err());
        } else {
            assert!(!directory.join("claude-mod").exists());
            assert!(!directory.join(MODE_FILE).exists());
        }
    }
}

#[test]
fn official_peer_body_escapes_its_own_closing_delimiter_before_turn_start() {
    let f = Fixture::new();
    let observed = f.frame().replacen(
        &f.message,
        &f.message
            .replace("</cross-session-message>", "<\\/cross-session-message>"),
        1,
    );
    f.send("turn.start", json!({"turnId":TURN,"text":observed}));
    f.complete("escaped body final");
    let events = f.events();
    assert_eq!(
        events.len(),
        1,
        "Claude escapes its own closing delimiter inside peer message data"
    );
    assert_eq!(events[0].message, "escaped body final");
}
