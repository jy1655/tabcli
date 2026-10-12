#[test]
fn declared_message_prepares_only_identity_and_leaves_pending_ownership_in_send() {
    use crate::native::delivery::TurnKind;
    let directory = tempfile::tempdir().unwrap();
    let surface = serde_json::from_value(
        serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
    )
    .unwrap();
    for kind in [TurnKind::Initial, TurnKind::FollowUp] {
        let context = super::super::transport_test_context(directory.path(), &surface, kind);
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
    let command =
        super::messenger_command(std::path::Path::new("unused"), directory, vec![]).unwrap();
    crate::native::process_env::tests::assert_provider_environment(
        &command,
        super::CLAUDE_CODE_SESSION_MARKERS,
        &[(
            crate::native::session::primitives::SESSION_DIR_ENV,
            Some(directory.as_os_str()),
        )],
    );
}

#[test]
fn git_probe_drops_caller_session_environment() {
    crate::native::process_env::tests::assert_helper_environment(
        &super::git_workspace_command(std::path::Path::new("/workspace")).unwrap(),
        &[],
    );
}

#[test]
fn workspace_trust_and_dialog_require_the_exact_workspace() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let homes = consent::fixture_homes(tmp.path());
    let native = consent::native_key(&workspace).unwrap();
    let key = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        native
    };
    super::super::super::write_json_atomic(
        &homes.claude,
        &serde_json::json!({"projects":{key.clone():{"hasTrustDialogAccepted":true}}}),
    )
    .unwrap();
    assert!(matches!(
        ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
        Trust::Trusted(_)
    ));
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &homes)
            .unwrap(),
        Trust::Absent
    );
    let screen = format!(
        "Accessing workspace:\n{key}\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\nClaude Code'll be able to read, edit, and execute files here.\nSecurity guide\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel"
    );
    assert_eq!(
        claude_trust_prompt_key(&screen, &workspace),
        Some(terminal::DialogKey::DownEnter)
    );
    assert_eq!(
        claude_trust_prompt_key(&screen, &workspace.join("child")),
        None
    );
    assert_eq!(
        claude_trust_prompt_key(&(screen.clone() + "\nAllow Bash?"), &workspace),
        None
    );
    // Claude 2.1.284 shows cwd but persists this dialog's decision at the
    // Git root. An exact child consent must never approve the parent repo.
    let child = workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let child_key = consent::native_key(&child).unwrap();
    let child_key = if cfg!(windows) {
        child_key.replace('\\', "/")
    } else {
        child_key
    };
    let child_screen = screen.replace(&key, &child_key);
    assert_eq!(claude_trust_prompt_key(&child_screen, &child), None);
    assert!(ADAPTER.workspace_trust(&child, &homes).is_err());
    assert!(matches!(
        ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
        Trust::Trusted(_)
    ));
    // A nested repository owns a distinct, exact approval scope.
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&child)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        claude_trust_prompt_key(&child_screen, &child),
        Some(terminal::DialogKey::DownEnter)
    );
    std::fs::write(&homes.claude, b"{\"projects\":[]}").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
}

use super::*;

#[test]
fn diagnostics_never_promote_configured_inbound_to_runtime_messaging() {
    use super::super::super::doctor::{Availability, Context};
    let directory = tempfile::tempdir().unwrap();
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"crossSessionInbound":"accept"}),
    ] {
        std::fs::write(
            directory.path().join("claude-settings.json"),
            settings.to_string(),
        )
        .unwrap();
        let checks = ADAPTER.diagnose(Context {
            directory: Some(directory.path()),
            manifest: None,
            executable: None,
            current_version: Some("2.1.280"),
            workspace: directory.path(),
            probe: true,
            deadline: Instant::now(),
        });
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "claude_messaging")
                .unwrap()
                .availability,
            Availability::Unknown
        );
        assert!(!directory.path().join("claude-pending-turn.json").exists());
    }
}

// Doctor lists the other live holders of a resumed conversation from the same registry
// scan the delivery boundaries use, and says nothing for a session that resumed none.
#[cfg(windows)]
#[test]
fn doctor_reports_other_live_holders_of_a_resumed_conversation() {
    use super::super::super::doctor::{Availability, Context};
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    std::fs::create_dir(&registry).unwrap();
    override_session_registry_for_test(Some(registry.clone()));
    let directory = root.path().join("session-reopendoc1");
    std::fs::create_dir(&directory).unwrap();
    let diagnose = |directory: &Path| {
        ADAPTER.diagnose(Context {
            directory: Some(directory),
            manifest: None,
            executable: None,
            current_version: Some("2.1.281"),
            workspace: directory,
            probe: false,
            deadline: Instant::now(),
        })
    };
    let holders_check = |directory: &Path| {
        diagnose(directory)
            .into_iter()
            .find(|check| check.id == "claude_resumed_conversation_holders")
    };
    // No manifest, then a manifest without resumed_from: no check at all.
    assert!(holders_check(&directory).is_none());
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::json!({"schema": 1, "id": "session-reopendoc1"}).to_string(),
    )
    .unwrap();
    assert!(holders_check(&directory).is_none());

    std::fs::write(
        directory.join("manifest.json"),
        serde_json::json!({
            "schema": 1,
            "id": "session-reopendoc1",
            "resumed_from": {
                "session": "session-reopendoc0",
                "provider_session_id": REOPEN_TEST_CONVERSATION,
                "event_id": "event-1-1.json",
            },
        })
        .to_string(),
    )
    .unwrap();
    let write_live_entry = |pid: u32, name: &str| {
        let identity = terminal::windows_process_identity(pid).unwrap();
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": REOPEN_TEST_CONVERSATION,
                "procStart": identity.creation_time.to_string(),
                "name": name,
            })
            .to_string(),
        )
        .unwrap();
    };
    // The managed process (this test process) is registered alone.
    write_live_entry(std::process::id(), "session-reopendoc1");
    let check = holders_check(&directory).unwrap();
    assert_eq!(check.availability, Availability::Available);
    assert_eq!(
        check.reason_code,
        "claude_resumed_conversation_exclusive_now"
    );

    // A foreign live resume is listed by pid.
    let mut foreign = std::process::Command::new("cmd.exe")
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    write_live_entry(foreign.id(), "foreign-resume");
    let check = holders_check(&directory).unwrap();
    let _ = foreign.kill();
    let _ = foreign.wait();
    assert_eq!(check.availability, Availability::Unavailable);
    assert_eq!(check.reason_code, "claude_resumed_conversation_shared");
    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["evidence"]["other_holders"],
        serde_json::json!([foreign.id()])
    );
    assert!(
        rendered["detail"]
            .as_str()
            .unwrap()
            .contains("gate reopen-conflict"),
        "{rendered}"
    );

    // A scan that cannot complete is reported as unknown, never as exclusive.
    std::fs::remove_file(registry.join(format!("{}.json", std::process::id()))).unwrap();
    let check = holders_check(&directory).unwrap();
    assert_eq!(check.availability, Availability::Unknown);
    assert_eq!(check.reason_code, "claude_resumed_conversation_unverified");
    override_session_registry_for_test(None);
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
fn managed_launch_name_is_the_known_unique_session_id() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let plan = ADAPTER
        .prepare_launch(LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: root.path(),
            title: "Human title",
            prompt: "review this",
        })
        .unwrap();

    assert!(
        plan.arguments
            .windows(2)
            .any(|pair| pair == ["--name", "session-safe123"])
    );
    assert!(
        !plan
            .arguments
            .iter()
            .any(|argument| argument == "Human title")
    );
}

#[test]
fn probe_and_version_queries_drop_the_same_markers_as_managed_launches() {
    use super::super::{FirstPartyCli, probe_environment_removals};
    assert_eq!(
        probe_environment_removals(FirstPartyCli::Claude),
        CLAUDE_CODE_SESSION_MARKERS
    );
    for marker in ["AI_AGENT", "CLAUDE_CODE_BRIDGE_SESSION_ID"] {
        assert!(probe_environment_removals(FirstPartyCli::Claude).contains(&marker));
    }
    for other in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert!(probe_environment_removals(other).is_empty(), "{other:?}");
    }
}

#[test]
fn managed_launch_drops_inherited_claude_code_session_markers() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    for windows in [false, true] {
        let plan = prepare_launch_for_platform(
            LaunchContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                workspace: root.path(),
                title: "Human title",
                prompt: "review this",
            },
            windows,
        )
        .unwrap();
        // The decisive marker: an inherited CLAUDE_CODE_CHILD_SESSION stops the managed
        // session from registering its cross-session inbox (issue #42).
        assert!(
            plan.environment_removals
                .contains(&"CLAUDE_CODE_CHILD_SESSION")
        );
        for marker in [
            "AI_AGENT",
            "CLAUDE_CODE_BRIDGE_SESSION_ID",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_PID",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_EFFORT",
        ] {
            assert!(plan.environment_removals.contains(&marker), "{marker}");
        }
        // User configuration is not the adapter's to strip.
        for kept in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_MODEL",
            "CLAUDE_CONFIG_DIR",
            "PATH",
            "HOME",
        ] {
            assert!(!plan.environment_removals.contains(&kept), "{kept}");
        }
    }
}

#[test]
fn session_settings_capture_inbound_policy_and_completion_hooks() {
    let settings = hook_settings(Path::new("/opt/Agent Bridge/bin/agent-bridge"));
    assert_eq!(settings["crossSessionInbound"], "accept");
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["command"],
        "/opt/Agent Bridge/bin/agent-bridge"
    );
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["args"],
        serde_json::json!(["native-hook", "claude"])
    );
    assert_eq!(
        settings["hooks"]["StopFailure"][0]["hooks"][0]["args"],
        serde_json::json!(["native-hook", "claude"])
    );
    assert!(settings["hooks"]["PermissionRequest"].is_null());
}

#[test]
fn every_platform_uses_the_official_cross_session_path_without_resume_fallback() {
    assert_eq!(
        ADAPTER.follow_up_transport().facts().name,
        "provider-cross-session-message"
    );
    assert_eq!(
        delivery::LAUNCH_ARGUMENT.facts().readiness,
        Readiness::LaunchArgument
    );
    assert_eq!(
        ClaudeMessage.facts().readiness,
        Readiness::AfterLaunch {
            ready_delay: Duration::from_secs(2)
        }
    );
}

#[test]
fn windows_launch_keeps_large_initial_prompts_out_of_argv() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-large123");
    std::fs::create_dir(&directory).unwrap();
    let prompt = "x".repeat(64 * 1024);
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: &directory,
            title: "large prompt",
            prompt: &prompt,
        },
        true,
    )
    .unwrap();

    assert!(!plan.prompt_is_positional);
    assert!(
        plan.arguments
            .iter()
            .all(|argument| !argument.to_string_lossy().contains(&prompt))
    );
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
fn correlated_response_requires_the_exact_final_turn_marker() {
    let pending = PendingCrossSessionTurn::new("claude-turn-safe123").unwrap();
    let exact = format!("completed response\n\n{}", pending.marker);
    assert_eq!(
        correlated_response(&exact, &pending).unwrap(),
        "completed response"
    );
    assert!(correlated_response("completed response", &pending).is_err());
    assert!(
        correlated_response(
            "completed response\n<!-- agent-bridge-claude-turn:other -->",
            &pending,
        )
        .is_err()
    );
}

#[test]
fn initial_stop_owns_the_official_claude_hook_payload_schema() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "initial result",
    });

    handle_hook(directory.path(), &payload).unwrap();

    let events = super::super::super::event_paths(directory.path()).unwrap();
    let event: crate::native::session::schema::SessionEvent =
        super::super::super::read_json(&events[0]).unwrap();
    assert_eq!(event.message, "initial result");
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some("claude-session-id")
    );
    assert_eq!(event.turn_id, None);
}

#[test]
fn delayed_uncorrelated_stop_cannot_complete_a_later_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let first_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    first_claim.retain();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "initial result",
    });
    handle_hook(directory.path(), &payload).unwrap();

    let later_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    later_claim.retain();
    super::super::super::update_status(directory.path(), SessionState::Claimed, None, None)
        .unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    handle_hook(directory.path(), &payload).unwrap();

    assert_eq!(
        super::super::super::event_paths(directory.path())
            .unwrap()
            .len(),
        1
    );
    assert!(
        directory
            .path()
            .join(super::super::super::TURN_CLAIM_FILE)
            .exists()
    );
}

#[test]
fn correlated_stop_records_only_the_expected_claude_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let pending = PendingCrossSessionTurn::new("claude-turn-safe123").unwrap();
    super::super::super::write_private(
        &directory.path().join(PENDING_TURN_FILE),
        &serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": format!("correlated result\n\n{}", pending.marker),
    });

    handle_hook(directory.path(), &payload).unwrap();

    let events = super::super::super::event_paths(directory.path()).unwrap();
    assert_eq!(events.len(), 1);
    let event: crate::native::session::schema::SessionEvent =
        super::super::super::read_json(&events[0]).unwrap();
    assert_eq!(event.message, "correlated result");
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some("claude-session-id")
    );
    assert_eq!(event.turn_id.as_deref(), Some("claude-turn-safe123"));
    assert!(directory.path().join(PENDING_TURN_FILE).is_file());
    assert!(
        !directory
            .path()
            .join(super::super::super::TURN_CLAIM_FILE)
            .exists()
    );
}

#[test]
fn unmarked_claude_stop_keeps_the_pending_turn_uncommitted() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let pending = PendingCrossSessionTurn::new("claude-turn-safe123").unwrap();
    super::super::super::write_private(
        &directory.path().join(PENDING_TURN_FILE),
        &serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "uncorrelated result",
    });

    handle_hook(directory.path(), &payload).unwrap();

    assert!(
        super::super::super::event_paths(directory.path())
            .unwrap()
            .is_empty()
    );
    assert!(directory.path().join(PENDING_TURN_FILE).is_file());
    assert!(
        directory
            .path()
            .join(super::super::super::TURN_CLAIM_FILE)
            .is_file()
    );
}

#[test]
fn stop_failure_does_not_claim_an_uncorrelated_pending_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    super::super::super::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let pending = PendingCrossSessionTurn::new("claude-turn-safe123").unwrap();
    super::super::super::write_private(
        &directory.path().join(PENDING_TURN_FILE),
        &serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "StopFailure",
        "session_id": "claude-session-id",
        "error": "rate_limit",
        "error_details": "429 Too Many Requests",
    });

    handle_hook(directory.path(), &payload).unwrap();

    assert!(
        super::super::super::event_paths(directory.path())
            .unwrap()
            .is_empty()
    );
    assert!(directory.path().join(PENDING_TURN_FILE).is_file());
    assert!(
        directory
            .path()
            .join(super::super::super::TURN_CLAIM_FILE)
            .is_file()
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

#[cfg(unix)]
#[test]
fn cross_session_process_never_receives_the_payload() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let directory = managed_session_directory(root.path());
    let executable = root.path().join("fake-claude");
    let pending = PendingCrossSessionTurn::new("claude-turn-safe123").unwrap();
    let expected_message = cross_session_target_message("follow-up secret", &pending);
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

    send_cross_session_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/opt/agent-bridge"),
        directory: &directory,
        provider_path: &executable,
        request_id: "claude-turn-safe123",
        prompt: "follow-up secret",
        deadline: Instant::now() + Duration::from_secs(10),
    })
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

    let error = send_cross_session_message_with_retry_policy(
        CrossSessionMessageContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            provider_path: &executable,
            request_id: "claude-turn-safe123",
            prompt: "follow-up secret",
            deadline: Instant::now() + Duration::from_secs(10),
        },
        Duration::from_millis(50),
        Duration::from_millis(10),
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

    let error = send_cross_session_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/opt/agent-bridge"),
        directory: &directory,
        provider_path: &executable,
        request_id: "claude-turn-safe123",
        prompt: "follow-up secret",
        deadline: Instant::now(),
    })
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

        MESSENGER_TIMEOUT_READY.with(|path| path.borrow_mut().take());
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
            "(\n",
            "  exec 3<>\"$PWD/descendant-pipe\"\n",
            "  : > \"$PWD/messenger-descendant-started\"\n",
            "  read -r finish <&3\n",
            "  : > \"$PWD/descendant-survived\"\n",
            ") &\n",
            "cat >/dev/null\n",
            "wait\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Setup has its own bounded budget; only possession of the FIFO triggers
    // expiry, so a delayed shell cannot let the teardown assertion pass early.
    MESSENGER_TIMEOUT_READY.with(|path| {
        assert!(
            path.borrow_mut()
                .replace(directory.join("messenger-descendant-started"))
                .is_none()
        );
    });
    let error = send_cross_session_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/opt/agent-bridge"),
        directory: &directory,
        provider_path: &executable,
        request_id: "claude-turn-safe123",
        prompt: "follow-up secret",
        deadline: Instant::now() + Duration::from_secs(5),
    })
    .unwrap_err();

    assert!(error.delivery_may_have_occurred());
    MESSENGER_TIMEOUT_READY.with(|path| {
        assert!(
            path.borrow().is_none(),
            "messenger timeout hook was not consumed"
        );
    });
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

    send_cross_session_message(CrossSessionMessageContext {
        bridge_executable: Path::new("/opt/agent-bridge"),
        directory: &directory,
        provider_path: &executable,
        request_id: "claude-turn-safe123",
        prompt: "follow-up secret",
        deadline: Instant::now() + Duration::from_secs(10),
    })
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

    send_cross_session_message_with_discovery_retry_policy(
        CrossSessionMessageContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            provider_path: &executable,
            request_id: "claude-turn-safe123",
            prompt: "follow-up secret",
            deadline: Instant::now() + Duration::from_secs(2),
        },
        Duration::from_millis(50),
        Duration::from_millis(10),
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

        let error = send_cross_session_message_with_retry_policy(
            CrossSessionMessageContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                provider_path: &executable,
                request_id: TEST_REQUEST,
                prompt: "follow-up secret",
                deadline: Instant::now() + Duration::from_secs(10),
            },
            Duration::from_millis(50),
            Duration::from_millis(10),
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

    let error = send_cross_session_message_with_retry_policy(
        CrossSessionMessageContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            provider_path: &executable,
            request_id: "claude-turn-safe123",
            prompt: "follow-up secret",
            deadline: Instant::now() + Duration::from_secs(10),
        },
        Duration::from_millis(50),
        Duration::from_millis(10),
    )
    .unwrap_err();

    assert!(!error.delivery_may_have_occurred());
    assert!(!directory.join(PENDING_TURN_FILE).exists());
}

const REOPEN_TEST_CONVERSATION: &str = "6928ca1c-1234-4abc-8def-0123456789ab";

#[test]
fn resume_plan_uses_the_official_resume_with_private_settings_and_managed_name_only() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session-reopenplan");
    std::fs::create_dir(&session).unwrap();
    let plan = prepare_resume_for_platform(
        ResumeContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &session,
            provider_session_id: REOPEN_TEST_CONVERSATION,
        },
        true,
    )
    .unwrap();
    let settings_path = session.join("claude-settings.json");
    assert_eq!(
        plan.arguments,
        vec![
            OsString::from("--resume"),
            OsString::from(REOPEN_TEST_CONVERSATION),
            OsString::from("--settings"),
            settings_path.clone().into_os_string(),
            OsString::from("--name"),
            OsString::from("session-reopenplan"),
        ]
    );
    assert!(matches!(plan.completion_monitor, CompletionMonitor::Hook));
    assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
    for forbidden in [
        "--dangerously-skip-permissions",
        "--model",
        "--effort",
        "--fork-session",
        "--session-id",
        "--print",
    ] {
        assert!(
            !plan.arguments.iter().any(|argument| argument == forbidden),
            "{forbidden} in resume plan"
        );
    }
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["crossSessionInbound"], "accept");
    assert!(settings["hooks"]["Stop"].is_array());
    assert!(settings["hooks"]["StopFailure"].is_array());
}

#[test]
fn resume_is_refused_off_native_windows_and_for_non_uuid_identity() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session-reopenplan2");
    std::fs::create_dir(&session).unwrap();
    let error = prepare_resume_for_platform(
        ResumeContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &session,
            provider_session_id: REOPEN_TEST_CONVERSATION,
        },
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with(
            "reopen unsupported: Claude reopen is implemented only for native Windows"
        ),
        "{error}"
    );
    assert!(!session.join("claude-settings.json").exists());
    assert_eq!(
        verify_reopen_available_for_platform(REOPEN_TEST_CONVERSATION, false)
            .unwrap_err()
            .to_string(),
        error.to_string()
    );
    for bad in [
        "",
        "not-a-uuid",
        "6928ca1c-1234-4abc-8def-0123456789ab-extra",
        "6928ca1c_1234_4abc_8def_0123456789ab",
        "../../../../../../../../../../etc/pw",
    ] {
        let error = verify_reopen_available_for_platform(bad, true).unwrap_err();
        assert!(
            error.to_string().contains("not a Claude session UUID"),
            "{bad:?}: {error}"
        );
    }
    verify_reopen_available_for_platform(REOPEN_TEST_CONVERSATION, true).unwrap();
    verify_reopen_available_for_platform("6928CA1C-1234-4ABC-8DEF-0123456789AB", true).unwrap();
}

#[test]
fn registry_entry_with_live_pid_and_matching_start_is_the_writer_and_stale_entries_are_not() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let entry = |pid: u32, session: &str, proc_start: serde_json::Value| {
        serde_json::json!({
            "pid": pid,
            "sessionId": session,
            "cwd": "D:\\Dev\\project",
            "startedAt": 1_790_231_893_451_u64,
            "procStart": proc_start,
            "version": "2.1.281",
            "name": "agent-bridge-a0",
            "status": "idle",
        })
        .to_string()
    };
    std::fs::write(
        registry.join("4242.json"),
        entry(4242, REOPEN_TEST_CONVERSATION, "134347054929920616".into()),
    )
    .unwrap();
    // The secret beside a registry entry is never opened: a directory in its place would
    // make any read attempt fail loudly.
    std::fs::create_dir(registry.join("4242.94cd6ac7033609ded4e5043f998182dc.key")).unwrap();
    std::fs::write(
        registry.join("4243.json"),
        entry(4243, "5e58ec26-0000-4000-8000-000000000000", "1".into()),
    )
    .unwrap();
    std::fs::write(registry.join("garbage.json"), "not json").unwrap();

    let live_and_matching = |pid: u32| Ok((pid == 4242).then_some(134_347_054_929_920_616_u64));
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live_and_matching).unwrap(),
        Some(4242)
    );
    assert_eq!(
        live_conversation_writer(
            registry,
            "5e58ec26-0000-4000-8000-000000000000",
            live_and_matching
        )
        .unwrap(),
        None
    );
    // A reused pid whose live creation time differs from procStart is not a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(Some(1))).unwrap(),
        None
    );
    // A dead pid is not a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(None)).unwrap(),
        None
    );
    // A live pid that cannot be inspected refuses instead of passing as stale.
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| {
        Err(anyhow::anyhow!("access denied"))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("4242"), "{error:#}");
    assert!(format!("{error:#}").contains("access denied"), "{error:#}");
    // No registry means no writer.
    assert_eq!(
        live_conversation_writer(&registry.join("missing"), REOPEN_TEST_CONVERSATION, |_| {
            Ok(Some(1))
        })
        .unwrap(),
        None
    );
    // A live pid without a verifiable procStart cannot be cleared.
    std::fs::write(
        registry.join("4245.json"),
        serde_json::json!({"pid": 4245, "sessionId": REOPEN_TEST_CONVERSATION}).to_string(),
    )
    .unwrap();
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |pid| {
        Ok((pid == 4245).then_some(7))
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("procStart is missing"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("live process 4245"),
        "{error:#}"
    );
}

// A record that deserializes is still judged by its file-name pid when its identity
// fields do not describe that file: the payload pid, the sessionId, and procStart are
// validated before the record is compared against any conversation.
#[test]
fn structurally_valid_records_with_inconsistent_identities_fail_closed_by_file_pid() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let fixtures: [(&str, serde_json::Value, &str); 8] = [
        (
            "8001.json",
            serde_json::json!({"pid": 8999, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "8001"}),
            "payload pid 8999 does not match the file name pid 8001",
        ),
        (
            "8002.json",
            serde_json::json!({"pid": 8002, "sessionId": "", "procStart": "8002"}),
            "sessionId \"\" is not a Claude session UUID",
        ),
        (
            "8003.json",
            serde_json::json!({"pid": 8003, "sessionId": "not-a-uuid", "procStart": "8003"}),
            "sessionId \"not-a-uuid\" is not a Claude session UUID",
        ),
        (
            "8004.json",
            serde_json::json!({"pid": 8004, "sessionId": REOPEN_TEST_CONVERSATION}),
            "procStart is missing",
        ),
        (
            "8005.json",
            serde_json::json!({"pid": 8005, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "later"}),
            "procStart \"later\" is not a process creation FILETIME",
        ),
        (
            "8006.json",
            serde_json::json!({"pid": 8006, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": []}),
            "procStart [] is not a process creation FILETIME",
        ),
        (
            "8007.json",
            serde_json::json!({"pid": 8007, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": -1}),
            "procStart -1 is not a process creation FILETIME",
        ),
        (
            "8008.json",
            serde_json::json!({"pid": 8008, "sessionId": "5e58ec26-0000-4000-8000-00000000000", "procStart": "8008"}),
            "is not a Claude session UUID",
        ),
    ];
    for (name, record, _) in &fixtures {
        std::fs::write(registry.join(name), record.to_string()).unwrap();
    }
    // Every inconsistent record names a dead file pid: none is a writer, for this
    // conversation or any other.
    for conversation in [
        REOPEN_TEST_CONVERSATION,
        "5e58ec26-0000-4000-8000-000000000000",
    ] {
        assert_eq!(
            live_conversation_writer(registry, conversation, |_| Ok(None)).unwrap(),
            None
        );
    }
    for (name, _, defect) in &fixtures {
        let file_pid: u32 = name.strip_suffix(".json").unwrap().parse().unwrap();
        // A live file pid refuses and names the record and its defect, whatever
        // conversation is asked and whatever the payload pid says.
        for conversation in [
            REOPEN_TEST_CONVERSATION,
            "5e58ec26-0000-4000-8000-000000000000",
        ] {
            let error = live_conversation_writer(registry, conversation, |candidate| {
                Ok((candidate == file_pid).then_some(u64::from(candidate)))
            })
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("reopen unsupported"), "{name}: {text}");
            assert!(text.contains(name), "{name}: {text}");
            assert!(text.contains(defect), "{name}: {text}");
            assert!(
                text.contains(&format!("live process {file_pid}")),
                "{name}: {text}"
            );
        }
        // An uninspectable file pid refuses as well.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            if candidate == file_pid {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains(name), "{name}: {text}");
        assert!(text.contains(defect), "{name}: {text}");
        assert!(text.contains("access denied"), "{name}: {text}");
    }
    // The reviewer's case in full: `<livePID>.json` whose payload names a dead pid is
    // refused by the live file pid, never cleared by the dead payload pid.
    let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
        Ok((candidate == 8001).then_some(1))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("8001.json"), "{error:#}");
    // A consistent record beside them is still recognized as the writer.
    std::fs::write(
        registry.join("8100.json"),
        serde_json::json!({"pid": 8100, "sessionId": REOPEN_TEST_CONVERSATION, "procStart": "8100"}).to_string(),
    )
    .unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            Ok((candidate == 8100).then_some(8100))
        })
        .unwrap(),
        Some(8100)
    );
    assert_eq!(
        validated_registry_record("8100", "{\"pid\": 8100, \"sessionId\": \"6928CA1C-1234-4ABC-8DEF-0123456789AB\", \"procStart\": 42, \"name\": \"x\"}")
            .unwrap()
            .proc_start,
        42
    );
}

// Records that cannot be parsed or lack a required field are judged by the pid in their
// file name: only a positively dead pid clears them.
#[test]
fn malformed_registry_records_fail_closed_unless_their_file_pid_is_dead() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let malformed: [(&str, &str); 4] = [
        ("5001.json", "{\"pid\": 5001, \"sessionId\": \"6928ca1c-12"),
        ("5002.json", "{\"pid\": 5002}"),
        (
            "5003.json",
            "{\"pid\": \"5003\", \"sessionId\": 7, \"procStart\": []}",
        ),
        ("5004.json", ""),
    ];
    for (name, text) in malformed {
        std::fs::write(registry.join(name), text).unwrap();
    }
    // Every malformed record names a dead pid: none is a writer.
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |_| Ok(None)).unwrap(),
        None
    );
    for (name, _) in malformed {
        let pid: u32 = name.strip_suffix(".json").unwrap().parse().unwrap();
        // A live file pid refuses and names the record, whatever conversation is asked.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            Ok((candidate == pid).then_some(1))
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("reopen unsupported"), "{text}");
        assert!(text.contains("is malformed"), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains(&format!("live process {pid}")), "{text}");
        let error = live_conversation_writer(
            registry,
            "5e58ec26-0000-4000-8000-000000000000",
            |candidate| Ok((candidate == pid).then_some(1)),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(name), "{error:#}");
        // An uninspectable file pid refuses as well.
        let error = live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |candidate| {
            if candidate == pid {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("is malformed"), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains("access denied"), "{text}");
    }
    // Non-numeric names are still not records at all: they are never judged by a pid.
    std::fs::write(registry.join("notes.json"), "").unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, |pid| {
            assert!((5001..=5004).contains(&pid), "judged {pid}");
            Ok(None)
        })
        .unwrap(),
        None
    );
}

// The pre-launch scan cannot reserve the conversation: a holder that registers after
// it must be caught by the same gate when it runs again at the launch boundary.
#[test]
fn a_holder_that_registers_after_the_initial_scan_is_caught_by_the_recheck() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let live = |pid: u32| Ok((pid == 6001).then_some(77_u64));
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live).unwrap(),
        None
    );
    std::fs::write(
        registry.join("6001.json"),
        serde_json::json!({
            "pid": 6001,
            "sessionId": REOPEN_TEST_CONVERSATION,
            "procStart": "77",
            "name": "foreign-resume",
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        live_conversation_writer(registry, REOPEN_TEST_CONVERSATION, live).unwrap(),
        Some(6001)
    );
}

#[test]
fn post_launch_check_reports_every_other_live_holder_once_the_reopened_process_registers() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let managed = "session-reopennew7";
    let write_entry = |pid: u32, session: &str, name: &str| {
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": session,
                "procStart": pid.to_string(),
                "name": name,
            })
            .to_string(),
        )
        .unwrap();
    };
    let live = |pid: u32| Ok(Some(u64::from(pid)));

    // Not registered yet: no answer, and the wait gives up at the deadline.
    assert_eq!(
        other_resumed_conversation_holders_once(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        None
    );
    let error = wait_for_resumed_conversation_holders(
        registry,
        REOPEN_TEST_CONVERSATION,
        managed,
        live,
        Instant::now() + Duration::from_millis(150),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not register"),
        "{error:#}"
    );

    // Registered alone, beside a stale entry of the same conversation and a live entry
    // of another conversation: exclusive.
    write_entry(7001, REOPEN_TEST_CONVERSATION, managed);
    write_entry(7002, "5e58ec26-0000-4000-8000-000000000000", "other-work");
    std::fs::write(
        registry.join("7003.json"),
        serde_json::json!({
            "pid": 7003,
            "sessionId": REOPEN_TEST_CONVERSATION,
            "procStart": "1",
            "name": "stale-closed-source",
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        wait_for_resumed_conversation_holders(
            registry,
            REOPEN_TEST_CONVERSATION,
            managed,
            live,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap(),
        Vec::<u32>::new()
    );

    // A foreign live resume of the same conversation is a detected conflict.
    write_entry(7004, REOPEN_TEST_CONVERSATION, "foreign-resume");
    assert_eq!(
        wait_for_resumed_conversation_holders(
            registry,
            REOPEN_TEST_CONVERSATION,
            managed,
            live,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap(),
        vec![7004]
    );
    // A live holder that could not be inspected refuses instead of passing.
    let error = wait_for_resumed_conversation_holders(
        registry,
        REOPEN_TEST_CONVERSATION,
        managed,
        |pid| {
            if pid == 7004 {
                Err(anyhow::anyhow!("access denied"))
            } else {
                Ok(Some(u64::from(pid)))
            }
        },
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("7004"), "{error:#}");

    // The delivery re-scan answers from the registry as it is now: a holder that
    // registered after the post-launch scan is reported, and it never waits.
    std::fs::remove_file(registry.join("7004.json")).unwrap();
    assert_eq!(
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        Vec::<u32>::new()
    );
    write_entry(7006, REOPEN_TEST_CONVERSATION, "late-foreign-resume");
    assert_eq!(
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap(),
        vec![7006]
    );
    std::fs::remove_file(registry.join("7006.json")).unwrap();
    // A managed registration that disappeared is a failure of the re-scan, not a clean
    // answer: the reopened process can no longer be shown to hold the conversation.
    std::fs::remove_file(registry.join("7001.json")).unwrap();
    let error =
        recheck_resumed_conversation_holders(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("is no longer registered"),
        "{error:#}"
    );
    write_entry(7001, REOPEN_TEST_CONVERSATION, managed);

    // Two live processes under the managed name cannot be told apart.
    write_entry(7005, REOPEN_TEST_CONVERSATION, managed);
    let error =
        other_resumed_conversation_holders_once(registry, REOPEN_TEST_CONVERSATION, managed, live)
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("2 live processes"),
        "{error:#}"
    );
}

#[cfg(windows)]
#[test]
fn windows_creation_time_check_distinguishes_the_live_process_from_a_stale_entry() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path();
    let pid = std::process::id();
    let live = live_process_creation_time(pid).unwrap().unwrap();
    let write_entry = |pid: u32, proc_start: u64| {
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": REOPEN_TEST_CONVERSATION,
                "procStart": proc_start.to_string(),
            })
            .to_string(),
        )
        .unwrap();
    };
    write_entry(pid, live);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        Some(pid)
    );
    write_entry(pid, live - 1);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        None
    );
    std::fs::remove_file(registry.join(format!("{pid}.json"))).unwrap();
    write_entry(4_294_967_294, live);
    assert_eq!(
        live_conversation_writer(
            registry,
            REOPEN_TEST_CONVERSATION,
            live_process_creation_time
        )
        .unwrap(),
        None
    );
}
