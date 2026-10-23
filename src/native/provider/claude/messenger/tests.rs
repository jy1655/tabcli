use super::*;
const PENDING_TURN_FILE: &str = "claude-pending-turn.json";
#[cfg(unix)]
use crate::native::provider::claude::turn::ClaudeTurn;
#[cfg(windows)]
use std::process::Command;
use std::time::Instant;

#[test]
fn declared_message_prepares_only_identity_and_leaves_pending_ownership_in_send() {
    use crate::native::delivery::TurnKind;
    let directory = tempfile::tempdir().unwrap();
    let surface = serde_json::from_value(
        serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
    )
    .unwrap();
    for kind in [TurnKind::Initial, TurnKind::FollowUp] {
        let context =
            crate::native::provider::transport_test_context(directory.path(), &surface, kind);
        let prepared = ClaudeMessage.prepare(&context).unwrap();
        assert!(
            prepared.prompt.is_empty(),
            "messenger must never receive raw Prepared.prompt"
        );
        assert!(
            prepared
                .correlation_id
                .as_deref()
                .unwrap()
                .starts_with("claude-turn-")
        );
        assert!(!directory.path().join(PENDING_TURN_FILE).exists());
        ClaudeMessage.discard_prepared(&context, Some(&prepared));
        assert!(!directory.path().join(PENDING_TURN_FILE).exists());
    }
}

#[test]
fn messenger_keeps_only_claude_removals_and_its_explicit_session_directory() {
    let directory = std::path::Path::new(".");
    let command = messenger_command(std::path::Path::new("unused"), directory, vec![]).unwrap();
    crate::native::process_env::tests::assert_provider_environment(
        &command,
        CLAUDE_CODE_SESSION_MARKERS,
        &[(
            crate::native::session::primitives::SESSION_DIR_ENV,
            Some(directory.as_os_str()),
        )],
    );
}

const TEST_REQUEST: &str = "claude-turn-safe123";
const TEST_REFERENCE: &str = "agent-bridge-payload:claude-turn-safe123";

fn test_guard(prompt: &str) -> MessageGuard {
    cross_session_message_plan(Path::new("/tmp/session-safe123"), TEST_REQUEST, prompt)
        .unwrap()
        .guard
}

fn managed_session_directory(root: &Path) -> PathBuf {
    let directory = root.join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    directory
}

fn test_files(directory: &Path) -> MessengerFiles {
    MessengerFiles::for_request(directory, TEST_REQUEST).unwrap()
}

fn write_guard(files: &MessengerFiles, guard: &MessageGuard) {
    std::fs::write(&files.guard, serde_json::to_vec(guard).unwrap()).unwrap();
}

fn send_message_hook_payload(event: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": event,
        "tool_name": "SendMessage",
        "tool_use_id": "send-1",
        "tool_input": {
            "to": "session-safe123",
            "summary": CROSS_SESSION_SUMMARY,
            "message": message,
        },
    })
}

// The messenger stream for one discovered SendMessage call that presented `message`.
fn messenger_trace(message: &str, send_result: &str) -> String {
    format!(
        concat!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"list-1\",\"name\":\"ListAgents\",\"input\":{{}}}}]}}}}\n",
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"list-1\",\"content\":\"session-safe123\"}}]}}}}\n",
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"send-1\",\"name\":\"SendMessage\",\"input\":{{\"to\":\"session-safe123\",\"summary\":{},\"message\":{}}}}}]}}}}\n",
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"send-1\",{}}}]}}}}\n",
            "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}\n"
        ),
        serde_json::to_string(CROSS_SESSION_SUMMARY).unwrap(),
        serde_json::to_string(message).unwrap(),
        send_result,
    )
}

const SEND_SUCCEEDED: &str = "\"content\":\"Message sent\"";
// What Claude reports for a call that this guard denied.
const SEND_DENIED: &str = "\"content\":\"Agent Bridge rejected a changed or unverifiable SendMessage reference\",\"is_error\":true";
const SEND_STOPPED: &str = "\"content\":\"Not run: the response that made this tool call was stopped by a safety classifier.\",\"is_error\":true";
const SEND_FAILED: &str =
    "\"content\":\"SendMessage failed while waiting for acknowledgement\",\"is_error\":true";

fn write_delivery_evidence(files: &MessengerFiles, tool_use_id: &str, verified: bool) {
    std::fs::write(
        &files.allowed,
        serde_json::to_vec(&MessageAllowed {
            tool_use_id: Some(tool_use_id.to_owned()),
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &files.receipt,
        serde_json::to_vec(&MessageReceipt {
            tool_use_id: Some(tool_use_id.to_owned()),
            verified,
        })
        .unwrap(),
    )
    .unwrap();
}

#[cfg(unix)]
fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

// A messenger stand-in that leaves the evidence the real hooks would record for one
// delivered call and then prints `trace`.
#[cfg(unix)]
fn delivering_messenger_script(trace: &str) -> String {
    let files = test_files(Path::new("."));
    format!(
        concat!(
            "printf '%s' '{{\"tool_use_id\":\"send-1\"}}' > \"$PWD/{}\"\n",
            "printf '%s' '{{\"tool_use_id\":\"send-1\",\"verified\":true}}' > \"$PWD/{}\"\n",
            "printf '%s' '{}'\n"
        ),
        file_name(&files.allowed),
        file_name(&files.receipt),
        trace.replace('\'', "'\"'\"'")
    )
}

#[test]
fn cross_session_plan_keeps_the_payload_away_from_the_messenger_and_restricts_tools() {
    let plan = cross_session_message_plan(
        Path::new("/tmp/session-safe123"),
        "claude-turn-safe123",
        "literal follow-up with --flags and 'quotes'",
    )
    .unwrap();
    let arguments = plan
        .arguments
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>();

    for required in [
        "--print",
        "--no-session-persistence",
        "--setting-sources",
        "--settings",
        "--permission-mode",
        "dontAsk",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "ListAgents,SendMessage",
    ] {
        assert!(arguments.iter().any(|value| value == required));
    }
    assert!(!arguments.iter().any(|value| value == "--safe-mode"));
    assert!(!arguments.iter().any(|value| value == "--allowedTools"));
    assert!(
        !arguments
            .iter()
            .any(|value| value.contains("literal follow-up"))
    );
    assert!(!plan.stdin.contains("literal follow-up"));
    assert_eq!(
        plan.files.settings,
        Path::new("/tmp/session-safe123").join("claude-message-settings.claude-turn-safe123.json")
    );
    assert!(arguments.windows(2).any(|pair| {
        pair[0] == "--settings" && Path::new(pair[1].as_ref()) == plan.files.settings
    }));
    let envelope: serde_json::Value = serde_json::from_str(&plan.stdin).unwrap();
    assert_eq!(envelope["recipient"], "session-safe123");
    assert_eq!(envelope["summary"], CROSS_SESSION_SUMMARY);
    assert_eq!(
        envelope["message"],
        "agent-bridge-payload:claude-turn-safe123"
    );
    assert_eq!(plan.guard.reference, envelope["message"]);
    assert_eq!(plan.guard.input.to, "session-safe123");
    assert_eq!(plan.guard.input.summary, CROSS_SESSION_SUMMARY);
    assert!(
        plan.guard
            .input
            .message
            .contains("literal follow-up with --flags and 'quotes'")
    );
    assert!(
        plan.guard
            .input
            .message
            .ends_with("<!-- agent-bridge-claude-turn:claude-turn-safe123 -->")
    );
}

#[test]
fn cross_session_stream_requires_discovery_exact_reference_and_success() {
    let trace = messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED);

    assert_eq!(
        confirm_messenger_stream(trace.as_bytes(), "session-safe123", TEST_REFERENCE).unwrap(),
        "send-1"
    );

    let wrong_reference = trace.replace(TEST_REFERENCE, "changed by the messenger");
    assert!(
        confirm_messenger_stream(
            wrong_reference.as_bytes(),
            "session-safe123",
            TEST_REFERENCE
        )
        .is_err()
    );
    let failed = messenger_trace(TEST_REFERENCE, SEND_DENIED);
    assert!(
        confirm_messenger_stream(failed.as_bytes(), "session-safe123", TEST_REFERENCE).is_err()
    );
    let undiscovered = trace.replace("\"name\":\"ListAgents\"", "\"name\":\"Other\"");
    assert!(
        confirm_messenger_stream(undiscovered.as_bytes(), "session-safe123", TEST_REFERENCE)
            .is_err()
    );
    let prefix_only = trace.replace("session-safe123", "session-safe1234");
    assert!(
        confirm_messenger_stream(prefix_only.as_bytes(), "session-safe123", TEST_REFERENCE)
            .is_err()
    );
}

#[test]
fn delivery_needs_the_guard_approval_and_the_executed_input_receipt() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let guard = test_guard("exact body");
    let trace = messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED);
    let confirm = || confirm_cross_session_delivery(trace.as_bytes(), &guard, &files);

    // The stream shows the reference the model presented, never the input that ran.
    assert!(confirm().is_err());

    write_delivery_evidence(&files, "send-1", false);
    assert!(confirm().is_err());

    write_delivery_evidence(&files, "send-1", true);
    confirm().unwrap();

    write_delivery_evidence(&files, "another-call", true);
    assert!(confirm().is_err());

    write_delivery_evidence(&files, "send-1", true);
    std::fs::remove_file(&files.allowed).unwrap();
    assert!(confirm().is_err());
}

#[test]
fn only_a_send_claude_reported_as_blocked_is_known_not_to_have_run() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let classify = |trace: &str| {
        unconfirmed_delivery_failure(trace.as_bytes(), &files, anyhow::anyhow!("unconfirmed"))
    };
    assert!(SEND_DENIED.contains(MESSAGE_GUARD_DENIAL_REASON));
    assert!(SEND_STOPPED.contains(PROVIDER_STOPPED_CALL_RESULT));
    let denied = messenger_trace(TEST_REFERENCE, SEND_DENIED);
    let stopped = messenger_trace("[Agent", SEND_STOPPED);
    let denied_in_text_blocks = messenger_trace(
        TEST_REFERENCE,
        &format!(
            "\"content\":[{{\"type\":\"text\",\"text\":\"{MESSAGE_GUARD_DENIAL_REASON}\"}}],\"is_error\":true"
        ),
    );
    let never_sent = "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}\n";
    for proven in [
        denied.as_str(),
        stopped.as_str(),
        denied_in_text_blocks.as_str(),
        never_sent,
    ] {
        let failure = classify(proven);
        assert!(!failure.delivery_may_have_occurred());
        assert!(failure.should_retry_discovery());
    }

    // An error that quotes a blocked-call report, or adds to it, is a different error.
    let error_result = |content: String| {
        messenger_trace(
            TEST_REFERENCE,
            &format!("\"content\":{content},\"is_error\":true"),
        )
    };
    let text_block = |text: &str| format!("{{\"type\":\"text\",\"text\":\"{text}\"}}");
    let quoting_reports = [MESSAGE_GUARD_DENIAL_REASON, PROVIDER_STOPPED_CALL_RESULT]
        .into_iter()
        .flat_map(|report| {
            [
                error_result(format!("\"SendMessage failed after dispatch: {report}\"")),
                error_result(format!("\"{report} Retried and failed after dispatch.\"")),
                error_result(format!(
                    "[{},{}]",
                    text_block(report),
                    text_block("This call failed after dispatch.")
                )),
                error_result(format!(
                    "[{},{{\"type\":\"image\",\"text\":\"ignored\"}}]",
                    text_block(report)
                )),
            ]
        })
        .collect::<Vec<_>>();
    for quoting in &quoting_reports {
        let failure = classify(quoting);
        assert!(failure.delivery_may_have_occurred());
        assert!(!failure.should_retry_discovery());
    }

    // Claude discards the output of a guard that outlives its hook timeout and runs the
    // call unguarded; the error it then reports is the tool's own and may follow a side
    // effect. The same holds for a reason reported without an error.
    let tool_error = messenger_trace(TEST_REFERENCE, SEND_FAILED);
    let reason_without_error = messenger_trace(
        TEST_REFERENCE,
        &SEND_DENIED.replace(",\"is_error\":true", ""),
    );
    let unguarded = messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED);
    let unanswered = denied.replace("\"tool_use_id\":\"send-1\"", "\"tool_use_id\":\"list-1\"");
    let contradicted = format!(
        "{denied}{}",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"send-1\",\"content\":\"Message sent\"}]}}\n"
    );
    let repeated = format!("{denied}{denied}");
    for uncertain in [
        tool_error.as_str(),
        reason_without_error.as_str(),
        unguarded.as_str(),
        unanswered.as_str(),
        contradicted.as_str(),
        repeated.as_str(),
        "not stream json\n",
    ] {
        let failure = classify(uncertain);
        assert!(failure.delivery_may_have_occurred());
        assert!(!failure.should_retry_discovery());
    }

    // An approved call may have run even when Claude reports a later call as blocked.
    for evidence in files.evidence() {
        std::fs::write(evidence, "{}").unwrap();
        assert!(classify(&denied).delivery_may_have_occurred());
        assert!(classify(never_sent).delivery_may_have_occurred());
        std::fs::remove_file(evidence).unwrap();
    }
}

#[test]
fn pre_tool_guard_replaces_the_reference_with_the_addressed_payload_once() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let guard = test_guard("exact body");
    write_guard(&files, &guard);
    let mut exact = send_message_hook_payload("PreToolUse", TEST_REFERENCE);
    // Observed messenger calls carry fields beyond the documented input; the replacement
    // input drops them.
    exact["tool_input"]["content"] = serde_json::json!(TEST_REFERENCE);

    assert_eq!(
        message_guard_decision(&files, TEST_REQUEST, &exact),
        MessageGuardDecision::Allow(guard.input.clone())
    );
    let allowed: MessageAllowed = read_message_evidence(&files.allowed).unwrap().unwrap();
    assert_eq!(allowed.tool_use_id.as_deref(), Some("send-1"));

    // One request can never deliver the payload twice.
    assert_eq!(
        message_guard_decision(&files, TEST_REQUEST, &exact),
        MessageGuardDecision::Deny
    );
}

#[test]
fn pre_tool_guard_denies_any_changed_or_unverifiable_send() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let guard = test_guard("exact body");
    write_guard(&files, &guard);
    let exact = send_message_hook_payload("PreToolUse", TEST_REFERENCE);

    let mut changed_address = exact.clone();
    changed_address["tool_input"]["to"] = serde_json::json!("session-safe123 [ref-1]");
    let mut changed_summary = exact.clone();
    changed_summary["tool_input"]["summary"] = serde_json::json!("changed summary");
    for denied in [
        send_message_hook_payload("PreToolUse", "agent-bridge-payload:claude-turn-other"),
        // The payload itself, whole or cut short, is not the reference.
        send_message_hook_payload("PreToolUse", &guard.input.message),
        send_message_hook_payload("PreToolUse", "[Agent"),
        send_message_hook_payload("PostToolUse", TEST_REFERENCE),
        changed_address,
        changed_summary,
    ] {
        assert_eq!(
            message_guard_decision(&files, TEST_REQUEST, &denied),
            MessageGuardDecision::Deny
        );
    }

    // A guard installed for one request never approves on behalf of another.
    let other_request = MessengerFiles::for_request(&directory, "claude-turn-other").unwrap();
    write_guard(&other_request, &guard);
    assert_eq!(
        message_guard_decision(&other_request, "claude-turn-other", &exact),
        MessageGuardDecision::Deny
    );

    let mut unrelated_payload = guard.clone();
    unrelated_payload.input.message = "payload for another turn".to_owned();
    write_guard(&files, &unrelated_payload);
    assert_eq!(
        message_guard_decision(&files, TEST_REQUEST, &exact),
        MessageGuardDecision::Deny
    );
    std::fs::remove_file(&files.guard).unwrap();
    assert_eq!(
        message_guard_decision(&files, TEST_REQUEST, &exact),
        MessageGuardDecision::Deny
    );
    assert!(!files.allowed.exists());
    assert!(!other_request.allowed.exists());
}

#[test]
fn receipt_verifies_only_the_executed_addressed_payload() {
    let guard = test_guard("exact body");
    let receipt_for = |payload: &serde_json::Value| {
        let root = tempfile::tempdir().unwrap();
        let directory = managed_session_directory(root.path());
        let files = test_files(&directory);
        write_guard(&files, &guard);
        record_message_receipt(&files, TEST_REQUEST, payload).unwrap();
        read_message_evidence::<MessageReceipt>(&files.receipt)
            .unwrap()
            .unwrap()
    };

    let executed = send_message_hook_payload("PostToolUse", &guard.input.message);
    let receipt = receipt_for(&executed);
    assert!(receipt.verified);
    assert_eq!(receipt.tool_use_id.as_deref(), Some("send-1"));

    // Claude ran the bare reference: the guard's replacement input was not applied.
    let unreplaced = send_message_hook_payload("PostToolUse", TEST_REFERENCE);
    assert!(!receipt_for(&unreplaced).verified);

    let mut unsuccessful = executed.clone();
    unsuccessful["tool_response"] = serde_json::json!({ "success": false });
    assert!(!receipt_for(&unsuccessful).verified);
}

#[test]
fn receipt_rejects_other_events_and_a_second_executed_send() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let guard = test_guard("exact body");
    write_guard(&files, &guard);

    let not_executed = send_message_hook_payload("PreToolUse", &guard.input.message);
    assert!(record_message_receipt(&files, TEST_REQUEST, &not_executed).is_err());
    assert!(!files.receipt.exists());

    let executed = send_message_hook_payload("PostToolUse", &guard.input.message);
    record_message_receipt(&files, TEST_REQUEST, &executed).unwrap();
    record_message_receipt(&files, TEST_REQUEST, &executed).unwrap();
    let receipt: MessageReceipt = read_message_evidence(&files.receipt).unwrap().unwrap();
    assert!(!receipt.verified);
}

#[test]
fn messenger_installs_scoped_send_message_hooks_and_cleans_them_up() {
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let files = test_files(&directory);
    let guard = test_guard("exact body");
    write_delivery_evidence(&files, "interrupted-attempt", true);

    let installed =
        install_message_guard(Path::new("/opt/agent-bridge"), TEST_REQUEST, &guard, &files)
            .unwrap();
    for evidence in files.evidence() {
        assert!(!evidence.exists());
    }
    let read_back = read_message_guard(&files, TEST_REQUEST).unwrap();
    assert_eq!(read_back.reference, TEST_REFERENCE);
    assert_eq!(read_back.input, guard.input);
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files.settings).unwrap()).unwrap();
    assert_eq!(settings["isolatePeerMachines"], true);
    for (event, action) in [
        ("PreToolUse", "message-guard"),
        ("PostToolUse", "message-receipt"),
    ] {
        let hook = &settings["hooks"][event][0];
        assert_eq!(hook["matcher"], "SendMessage");
        assert_eq!(hook["hooks"][0]["command"], "/opt/agent-bridge");
        assert_eq!(
            hook["hooks"][0]["args"],
            serde_json::json!(["native-provider-control", "claude", action, TEST_REQUEST])
        );
    }

    write_delivery_evidence(&files, "send-1", true);
    drop(installed);
    for path in [&files.guard, &files.settings]
        .into_iter()
        .chain(files.evidence())
    {
        assert!(!path.exists());
    }
}

#[test]
fn cleanup_after_an_early_target_completion_leaves_the_next_request_intact() {
    // The target can complete a delivered turn before its sender stops settling, so the
    // next request may install and approve while the previous cleanup is still pending.
    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let settling = MessengerFiles::for_request(&directory, "claude-turn-settling").unwrap();
    let settling_guard = cross_session_message_plan(&directory, "claude-turn-settling", "A")
        .unwrap()
        .guard;
    let settling_installed = install_message_guard(
        Path::new("/opt/agent-bridge"),
        "claude-turn-settling",
        &settling_guard,
        &settling,
    )
    .unwrap();

    let next = test_files(&directory);
    let next_guard = cross_session_message_plan(&directory, TEST_REQUEST, "B")
        .unwrap()
        .guard;
    let _next_installed = install_message_guard(
        Path::new("/opt/agent-bridge"),
        TEST_REQUEST,
        &next_guard,
        &next,
    )
    .unwrap();
    let exact = send_message_hook_payload("PreToolUse", TEST_REFERENCE);
    assert_eq!(
        message_guard_decision(&next, TEST_REQUEST, &exact),
        MessageGuardDecision::Allow(next_guard.input.clone())
    );
    let executed = send_message_hook_payload("PostToolUse", &next_guard.input.message);
    record_message_receipt(&next, TEST_REQUEST, &executed).unwrap();

    drop(settling_installed);

    assert!(next.guard.exists() && next.settings.exists());
    confirm_executed_payload(&next, "send-1").unwrap();
    // The approval survived, so the next request still cannot be approved twice.
    assert_eq!(
        message_guard_decision(&next, TEST_REQUEST, &exact),
        MessageGuardDecision::Deny
    );
}

#[test]
fn native_messenger_pending_matches_the_guard_executed_input() {
    use crate::native::session::{SessionState, Store, turn};

    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    crate::native::session::update_status(&directory, SessionState::Working, None, None).unwrap();
    let store = Store::open_unchecked(&directory);
    let claim = turn::claim(&store, &[]).unwrap();
    let token = claim.token().to_owned();
    store
        .private("claude-completion-mode.json")
        .write_json(&serde_json::json!({
            "schema":1,"mode":"native_v1","reason":"fixture","claude_version":"2.1.302"
        }))
        .unwrap();
    std::fs::write(
        directory.join("messenger.mjs"),
        r#"
import fs from 'node:fs';
const envelope = fs.readFileSync(0, 'utf8');
fs.writeFileSync('messenger-envelope.tmp', envelope);
fs.renameSync('messenger-envelope.tmp', 'messenger-envelope.json');
const deadline = Date.now() + 10000;
const timer = setInterval(() => {
  if (fs.existsSync('messenger-stream.jsonl')) {
    process.stdout.write(fs.readFileSync('messenger-stream.jsonl'));
    clearInterval(timer);
  } else if (Date.now() >= deadline) {
    clearInterval(timer);
    process.exitCode = 1;
  }
}, 10);
"#,
    )
    .unwrap();
    #[cfg(windows)]
    let executable = {
        let path = directory.join("fake-claude.cmd");
        std::fs::write(&path, "@node \"%~dp0messenger.mjs\"\r\n").unwrap();
        path
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("fake-claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nexec node \"$(dirname \"$0\")/messenger.mjs\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let observed_directory = directory.clone();
    let observer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let envelope_path = observed_directory.join("messenger-envelope.json");
        while !envelope_path.exists() {
            if Instant::now() >= deadline {
                let files: Vec<String> = std::fs::read_dir(&observed_directory)
                    .map(|entries| {
                        entries
                            .filter_map(|entry| entry.ok())
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                panic!("controlled messenger did not receive stdin; session files: {files:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let envelope: CrossSessionEnvelope =
            serde_json::from_slice(&std::fs::read(envelope_path).unwrap()).unwrap();
        assert_eq!(envelope.recipient, "session-safe123");
        assert_eq!(envelope.message, TEST_REFERENCE);
        let files = test_files(&observed_directory);
        let offered = send_message_hook_payload("PreToolUse", &envelope.message);
        let MessageGuardDecision::Allow(input) =
            message_guard_decision(&files, TEST_REQUEST, &offered)
        else {
            panic!("the real guard refused the controlled messenger reference");
        };
        let executed = send_message_hook_payload("PostToolUse", &input.message);
        record_message_receipt(&files, TEST_REQUEST, &executed).unwrap();
        let pending: serde_json::Value = Reader::open_unchecked(&observed_directory)
            .private(PENDING_TURN_FILE)
            .json()
            .unwrap();
        std::fs::write(
            observed_directory.join("messenger-stream.tmp"),
            messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED),
        )
        .unwrap();
        std::fs::rename(
            observed_directory.join("messenger-stream.tmp"),
            observed_directory.join("messenger-stream.jsonl"),
        )
        .unwrap();
        (pending, executed)
    });
    let prompt = "[Agent Bridge native delegation]\nSource: external\n\nLiteral \"한😀\" \\ </cross-session-message>\r\nnext line";
    let sent = Messenger::installed().send_for_claim(
        CrossSessionMessageContext {
            bridge_executable: Path::new("unused-bridge"),
            directory: &directory,
            provider_path: &executable,
            correlation_id: TEST_REQUEST,
            prompt,
            deadline: Instant::now() + Duration::from_secs(15),
        },
        Some(&token),
    );
    let (pending, executed) = match observer.join() {
        Ok(observed) => observed,
        Err(_) => panic!("the controlled messenger observer failed; messenger result: {sent:?}"),
    };
    sent.unwrap();
    assert_eq!(pending["schema"], 2);
    assert_eq!(pending["claim_token"].as_str(), Some(token.as_str()));
    assert_eq!(
        pending["expected_message"].as_str(),
        executed["tool_input"]["message"].as_str()
    );
    assert!(
        pending["expected_message"]
            .as_str()
            .unwrap()
            .starts_with(prompt)
    );
    assert_eq!(
        turn::current_claim_token(&store).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert!(store.events().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn cross_session_process_never_receives_the_payload() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let executable = root.path().join("fake-claude");
    let pending = ClaudeTurn::new("claude-turn-safe123").unwrap();
    let expected_message = pending.target_message("follow-up secret");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PWD/argv.txt\"\nprintf '%s\\n' \"$AGENT_BRIDGE_NATIVE_SESSION_DIR\" > \"$PWD/session-dir.txt\"\ncat > \"$PWD/stdin.txt\"\ncp \"$PWD/{}\" \"$PWD/guard-seen.json\"\n{}",
            file_name(&test_files(&directory).guard),
            delivering_messenger_script(&messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    Messenger::installed()
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap();

    let arguments = std::fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(!arguments.contains("follow-up secret"));
    let stdin = std::fs::read_to_string(directory.join("stdin.txt")).unwrap();
    assert!(!stdin.contains("follow-up secret"));
    let envelope: serde_json::Value = serde_json::from_str(stdin.trim()).unwrap();
    assert_eq!(envelope["recipient"], "session-safe123");
    assert_eq!(envelope["message"], TEST_REFERENCE);
    let guard: MessageGuard =
        serde_json::from_str(&std::fs::read_to_string(directory.join("guard-seen.json")).unwrap())
            .unwrap();
    assert_eq!(guard.input.message, expected_message);
    assert_eq!(
        std::fs::read_to_string(directory.join("session-dir.txt"))
            .unwrap()
            .trim(),
        directory.to_string_lossy()
    );
    assert!(directory.join(PENDING_TURN_FILE).is_file());
    let files = test_files(&directory);
    for path in [&files.guard, &files.settings]
        .into_iter()
        .chain(files.evidence())
    {
        assert!(!path.exists());
    }
}

#[cfg(unix)]
#[test]
fn messenger_failure_without_send_is_known_not_delivered() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let executable = root.path().join("fake-claude");
    std::fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nexit 1\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut messenger = Messenger {
        clock: SystemClock,
        retry: RetryPolicy {
            window: Duration::from_millis(50),
            delay: Duration::from_millis(10),
        },
    };
    let error = messenger
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap_err();

    assert!(!error.delivery_may_have_occurred());
    assert!(!directory.join(PENDING_TURN_FILE).exists());
}

#[cfg(unix)]
#[test]
fn expired_cross_session_deadline_never_starts_the_messenger() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let executable = root.path().join("fake-claude");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf started > \"$PWD/messenger-started\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    let error = Messenger::installed()
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now(),
            },
            None,
        )
        .unwrap_err();

    assert!(!error.delivery_may_have_occurred());
    assert!(!directory.join("messenger-started").exists());
    assert!(!directory.join(PENDING_TURN_FILE).exists());
}

#[cfg(unix)]
struct MessengerTimeoutFixture {
    pipe: PathBuf,
}

#[cfg(unix)]
impl Drop for MessengerTimeoutFixture {
    fn drop(&mut self) {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};

        // Release a surviving descendant even if an earlier assertion failed.
        if let Ok(mut writer) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pipe)
        {
            let _ = writer.write_all(b"stop\n");
        }
    }
}

// Real time until the fixture's descendant holds the FIFO (the ready file exists), the
// context's deadline from then on. Setup keeps its real budget: a descendant that never
// starts is the ordinary real-time timeout, with `expiry_forced` still false.
#[cfg(unix)]
struct ReadyFileClock {
    ready: PathBuf,
    deadline: Instant,
    expiry_forced: bool,
}

#[cfg(unix)]
impl Clock for ReadyFileClock {
    fn now(&mut self) -> Instant {
        if self.expiry_forced || self.ready.is_file() {
            self.expiry_forced = true;
            self.deadline
        } else {
            Instant::now()
        }
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[cfg(unix)]
#[test]
fn messenger_timeout_terminates_descendants_before_they_can_act_late() {
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    };

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let pipe = directory.join("descendant-pipe");
    let pipe_name = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(pipe_name.as_ptr(), 0o600) }, 0);
    let _fixture = MessengerTimeoutFixture { pipe: pipe.clone() };
    let executable = root.path().join("fake-claude");
    std::fs::write(
        &executable,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "(\n",
            "  exec 3<>\"$PWD/descendant-pipe\"\n",
            "  : > \"$PWD/messenger-descendant-started\"\n",
            "  read -r finish <&3\n",
            "  : > \"$PWD/descendant-survived\"\n",
            ") &\n",
            "wait\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Setup has its own bounded budget; only possession of the FIFO triggers
    // expiry, so a delayed shell cannot let the teardown assertion pass early. The
    // fixture reads its input before it forks, so the ready file cannot exist at the
    // messenger's pre-input check: expiry is forced only inside the wait loop.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut messenger = Messenger {
        clock: ReadyFileClock {
            ready: directory.join("messenger-descendant-started"),
            deadline,
            expiry_forced: false,
        },
        retry: RetryPolicy::installed(),
    };
    let error = messenger
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline,
            },
            None,
        )
        .unwrap_err();

    assert!(error.delivery_may_have_occurred());
    assert!(
        messenger.clock.expiry_forced,
        "the messenger expired on real time before the fixture's descendant held its FIFO"
    );
    let reason = format!("{:#}", error.into_error());
    assert!(
        reason.contains("Claude cross-session messenger timed out"),
        "{reason}"
    );
    assert!(directory.join("messenger-descendant-started").is_file());
    // Even a zombie has closed its FIFO. Observe teardown directly rather
    // than guessing how long a surviving descendant would take to act.
    let stopped = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pipe)
        {
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => break,
            Ok(writer) => drop(writer),
            Err(error) => panic!("failed to inspect descendant FIFO: {error}"),
        }
        assert!(
            Instant::now() < stopped,
            "timed-out descendant still holds its FIFO"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!directory.join("descendant-survived").exists());
}

#[cfg(windows)]
#[test]
fn windows_messenger_cannot_run_before_job_assignment() {
    let mut command = Command::new(std::env::var_os("ComSpec").unwrap());
    command
        .args(["/d", "/c", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_process_tree(&mut command);

    let mut child = command.spawn().unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "messenger executed before it could be assigned to the containment job"
    );
    let process_tree = ProviderProcessTree::attach(&child).unwrap();
    terminate_child_tree(&mut child, &process_tree);
}

#[cfg(windows)]
#[test]
fn windows_messenger_runs_only_after_containment_resume() {
    let mut command = Command::new(std::env::var_os("ComSpec").unwrap());
    command
        .args(["/d", "/c", "exit", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_process_tree(&mut command);

    let mut child = command.spawn().unwrap();
    let process_tree = ProviderProcessTree::attach(&child).unwrap();
    process_tree.resume(&child).unwrap();
    assert!(child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn messenger_retries_only_a_proven_pre_send_discovery_miss() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let executable = root.path().join("fake-claude");
    std::fs::write(
        &executable,
        format!(
            concat!(
                "#!/bin/sh\n",
                "cat >/dev/null\n",
                "printf x >> \"$PWD/attempts\"\n",
                "if [ \"$(wc -c < \"$PWD/attempts\")\" -eq 1 ]; then\n",
                "  printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}'\n",
                "  exit 0\n",
                "fi\n",
                "{}"
            ),
            delivering_messenger_script(&messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    Messenger::installed()
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(directory.join("attempts")).unwrap(),
        "xx"
    );
}

#[cfg(unix)]
#[test]
fn messenger_retry_window_starts_at_the_first_proven_discovery_miss() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let executable = root.path().join("fake-claude");
    std::fs::write(
        &executable,
        format!(
            concat!(
                "#!/bin/sh\n",
                "cat >/dev/null\n",
                "printf x >> \"$PWD/attempts\"\n",
                "if [ \"$(wc -c < \"$PWD/attempts\")\" -eq 1 ]; then\n",
                "  sleep 0.2\n",
                "  printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}'\n",
                "  exit 0\n",
                "fi\n",
                "{}"
            ),
            delivering_messenger_script(&messenger_trace(TEST_REFERENCE, SEND_SUCCEEDED))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut messenger = Messenger {
        clock: SystemClock,
        retry: RetryPolicy {
            window: Duration::from_millis(50),
            delay: Duration::from_millis(10),
        },
    };
    messenger
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(2),
            },
            None,
        )
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(directory.join("attempts")).unwrap(),
        "xx"
    );
}

#[cfg(unix)]
#[test]
fn only_a_send_reported_as_blocked_is_retried_and_released() {
    use std::os::unix::fs::PermissionsExt;

    // Observed 2026-09-18 with Claude Code 2.1.276-2.1.278: the provider stopped the
    // messenger response while it was still writing the SendMessage input, and Claude
    // reported the truncated call as not run. A tool error that is not such a report may
    // follow a side effect, so it must neither be retried nor release the turn.
    for (send_input, send_result, blocked) in [
        ("[Agent", SEND_STOPPED, true),
        (TEST_REFERENCE, SEND_FAILED, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = managed_session_directory(root.path());
        let executable = root.path().join("fake-claude");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf x >> \"$PWD/attempts\"\nprintf '%s' '{}'\n",
                messenger_trace(send_input, send_result).replace('\'', "'\"'\"'")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut messenger = Messenger {
            clock: SystemClock,
            retry: RetryPolicy {
                window: Duration::from_millis(50),
                delay: Duration::from_millis(10),
            },
        };
        let error = messenger
            .send_for_claim(
                CrossSessionMessageContext {
                    bridge_executable: Path::new("/opt/agent-bridge"),
                    directory: &directory,
                    provider_path: &executable,
                    correlation_id: TEST_REQUEST,
                    prompt: "follow-up secret",
                    deadline: Instant::now() + Duration::from_secs(10),
                },
                None,
            )
            .unwrap_err();

        let attempts = std::fs::read_to_string(directory.join("attempts")).unwrap();
        assert_eq!(error.delivery_may_have_occurred(), !blocked);
        assert_eq!(attempts.len() > 1, blocked);
        assert_eq!(directory.join(PENDING_TURN_FILE).exists(), !blocked);
    }
}

#[cfg(unix)]
#[test]
fn successful_messenger_trace_without_send_is_not_classified_as_delivered() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let executable = root.path().join("fake-claude");
    std::fs::write(
        &executable,
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "printf '%s\\n' ",
            "'{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut messenger = Messenger {
        clock: SystemClock,
        retry: RetryPolicy {
            window: Duration::from_millis(50),
            delay: Duration::from_millis(10),
        },
    };
    let error = messenger
        .send_for_claim(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                correlation_id: "claude-turn-safe123",
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(10),
            },
            None,
        )
        .unwrap_err();

    assert!(!error.delivery_may_have_occurred());
    assert!(!directory.join(PENDING_TURN_FILE).exists());
}

#[test]
fn messenger_output_reader_keeps_a_full_budget_for_each_stream() {
    use std::io::Write;

    assert_eq!(MAX_CROSS_SESSION_OUTPUT_BYTES, 1_048_576);
    let stdout_bytes = vec![b'a'; MAX_CROSS_SESSION_OUTPUT_BYTES];
    let stderr_bytes = vec![b'b'; MAX_CROSS_SESSION_OUTPUT_BYTES];
    let mut stdout = tempfile::tempfile().unwrap();
    stdout.write_all(&stdout_bytes).unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    stderr.write_all(&stderr_bytes).unwrap();

    let (stdout, stdout_truncated) = read_capped_file(&mut stdout).unwrap();
    let (stderr, stderr_truncated) = read_capped_file(&mut stderr).unwrap();
    assert_eq!(stdout, stdout_bytes);
    assert_eq!(stderr, stderr_bytes);
    assert!(!stdout_truncated);
    assert!(!stderr_truncated);
    assert_eq!(
        stdout.len() + stderr.len(),
        2 * MAX_CROSS_SESSION_OUTPUT_BYTES
    );

    let oversized_bytes = vec![b'c'; MAX_CROSS_SESSION_OUTPUT_BYTES + 1];
    let mut oversized = tempfile::tempfile().unwrap();
    oversized.write_all(&oversized_bytes).unwrap();
    let (output, truncated) = read_capped_file(&mut oversized).unwrap();
    assert_eq!(output, oversized_bytes[..MAX_CROSS_SESSION_OUTPUT_BYTES]);
    assert!(truncated);
}
