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

// The watcher of a plan whose completion arrives through the hook alone: starting it on a
// directory that does not exist succeeds, stopping it succeeds, and the directory is left
// as it was, so nothing ran.
fn assert_no_op_watcher(watcher: Box<dyn crate::native::provider::CompletionWatcher>) {
    assert_eq!(format!("{watcher:?}"), "NoWatcher");
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-session");
    let active = watcher.start(&absent).unwrap();
    active.stop().unwrap();
    assert!(!absent.exists());
}

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

    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["detail"],
        format!(
            "No other live Claude Code process is registered for resumed conversation {REOPEN_TEST_CONVERSATION} at this moment."
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION, "other_holders": []})
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

    assert_eq!(
        rendered["detail"],
        format!(
            "Resumed conversation {REOPEN_TEST_CONVERSATION} is also held by live Claude Code process(es) {}; the next tell to this session will be refused with gate reopen-conflict while they stay registered.",
            foreign.id()
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION, "other_holders": [foreign.id()]})
    );

    // A scan that cannot complete is reported as unknown, never as exclusive.
    std::fs::remove_file(registry.join(format!("{}.json", std::process::id()))).unwrap();
    let check = holders_check(&directory).unwrap();
    assert_eq!(check.availability, Availability::Unknown);
    assert_eq!(check.reason_code, "claude_resumed_conversation_unverified");
    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["detail"],
        format!(
            "The registry scan for resumed conversation {REOPEN_TEST_CONVERSATION} failed: reopened Claude session session-reopendoc1 is no longer registered for conversation {REOPEN_TEST_CONVERSATION} under {}; the next tell to this session will be refused with gate reopen-verification-failed until it succeeds.",
            registry.display()
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION})
    );
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
            yolo: false,
            model: None,
            effort: None,
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
                yolo: false,
                model: None,
                effort: None,
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
            yolo: false,
            model: None,
            effort: None,
        },
        true,
    )
    .unwrap();

    assert_eq!(
        plan.initial_prompt,
        InitialPromptDisposition::AwaitingDelivery
    );
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
        correlation_id: "claude-turn-safe123",
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
            correlation_id: "claude-turn-safe123",
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
        correlation_id: "claude-turn-safe123",
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
        correlation_id: "claude-turn-safe123",
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
        correlation_id: "claude-turn-safe123",
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
            correlation_id: "claude-turn-safe123",
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
                correlation_id: TEST_REQUEST,
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
            correlation_id: "claude-turn-safe123",
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

pub(super) const REOPEN_TEST_CONVERSATION: &str = "6928ca1c-1234-4abc-8def-0123456789ab";

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
            yolo: false,
            model: None,
            effort: None,
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
    assert_no_op_watcher(plan.watcher);
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
            yolo: false,
            model: None,
            effort: None,
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
fn reopen_bundle_exists_on_every_platform() {
    assert!(ADAPTER.reopen().is_ok());
}

#[test]
fn doctor_empty_registry_preserves_unverified_detail_and_omission() {
    use super::super::super::doctor::Availability;
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry");
    std::fs::create_dir(&registry).unwrap();
    override_session_registry_for_test(Some(registry.clone()));
    for name in ["session-doctorempty", "invalid"] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        assert!(resumed_conversation_holders_check(&directory).is_none());
        for manifest in [
            serde_json::json!({}),
            serde_json::json!({"resumed_from": {}}),
            serde_json::json!({"resumed_from": {"provider_session_id": 1}}),
        ] {
            std::fs::write(directory.join("manifest.json"), manifest.to_string()).unwrap();
            assert!(resumed_conversation_holders_check(&directory).is_none());
        }
        for conversation in [REOPEN_TEST_CONVERSATION, "not-a-uuid"] {
            std::fs::write(
                directory.join("manifest.json"),
                serde_json::json!({"resumed_from": {"provider_session_id": conversation}})
                    .to_string(),
            )
            .unwrap();
            let check = resumed_conversation_holders_check(&directory);
            if name == "invalid" {
                assert!(check.is_none());
                continue;
            }
            let check = check.unwrap();
            assert_eq!(check.id, "claude_resumed_conversation_holders");
            assert_eq!(check.availability, Availability::Unknown);
            assert_eq!(check.reason_code, "claude_resumed_conversation_unverified");
            let rendered = serde_json::to_value(&check).unwrap();
            assert_eq!(
                rendered["detail"],
                format!(
                    "The registry scan for resumed conversation {conversation} failed: reopened Claude session {name} is no longer registered for conversation {conversation} under {}; the next tell to this session will be refused with gate reopen-verification-failed until it succeeds.",
                    registry.display()
                )
            );
            assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
            assert_eq!(
                rendered["evidence"],
                serde_json::json!({"provider_session_id": conversation})
            );
        }
    }
    override_session_registry_for_test(None);
}

const DOCTOR_HOLDER_NEXT_ACTION: &str = "Detection is best-effort and taken only at launch, before each delivery, and now; a foreign claude --resume can still interleave between checks. Close the other holder before the next tell, or accept the shared transcript.";

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

// The expected policy prefix of a Claude command, in the fixed order the adapter emits it:
// the bypass only when requested, then effort, then model.
fn claude_policy(yolo: bool, model: Option<&str>, effort: Option<&str>) -> Vec<OsString> {
    let mut expected = Vec::new();
    if yolo {
        expected.push(OsString::from("--dangerously-skip-permissions"));
    }
    if let Some(effort) = effort {
        expected.extend([OsString::from("--effort"), OsString::from(effort)]);
    }
    if let Some(model) = model {
        expected.extend([OsString::from("--model"), OsString::from(model)]);
    }
    expected
}

// The complete launch command for every policy combination and both platform branches:
// policy, private settings, managed name, and the prompt last off Windows only.
#[test]
fn launch_plan_is_the_complete_command_for_every_policy_and_platform() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    let settings_path = directory.join("claude-settings.json");
    for windows in [false, true] {
        for yolo in [false, true] {
            for (model, mapped) in [(None, None), (Some("Fable5"), Some("Fable"))] {
                for effort in [None, Some("high")] {
                    let _ = std::fs::remove_file(&settings_path);
                    let plan = prepare_launch_for_platform(
                        LaunchContext {
                            bridge_executable: Path::new("/opt/agent-bridge"),
                            directory: &directory,
                            workspace: root.path(),
                            title: "Claude test",
                            prompt: "claude prompt",
                            yolo,
                            model,
                            effort,
                        },
                        windows,
                    )
                    .unwrap();
                    let label =
                        format!("windows={windows} yolo={yolo} model={model:?} effort={effort:?}");
                    let mut expected = claude_policy(yolo, mapped, effort);
                    expected.extend([
                        OsString::from("--settings"),
                        settings_path.clone().into_os_string(),
                        OsString::from("--name"),
                        OsString::from("session-safe123"),
                    ]);
                    if !windows {
                        expected.push(OsString::from("claude prompt"));
                    }
                    assert_eq!(plan.arguments, expected, "{label}");
                    assert_eq!(
                        plan.initial_prompt,
                        if windows {
                            InitialPromptDisposition::AwaitingDelivery
                        } else {
                            InitialPromptDisposition::AcceptedAtSpawn
                        },
                        "{label}"
                    );
                    assert_no_op_watcher(plan.watcher);
                    assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
                    let settings: serde_json::Value =
                        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap())
                            .unwrap();
                    assert_eq!(settings["crossSessionInbound"], "accept", "{label}");
                    assert_eq!(
                        settings["hooks"]["Stop"][0]["hooks"][0]["command"], "/opt/agent-bridge",
                        "{label}"
                    );
                }
            }
        }
    }
}

// The complete resume command carries the new request's policy ahead of the official
// resume arguments and never a prompt; the reopened session awaits delivery.
#[test]
fn resume_plan_is_the_complete_command_for_every_policy() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session-reopenplan");
    std::fs::create_dir(&session).unwrap();
    let settings_path = session.join("claude-settings.json");
    for yolo in [false, true] {
        for (model, mapped) in [(None, None), (Some("Fable5"), Some("Fable"))] {
            for effort in [None, Some("max")] {
                let plan = prepare_resume_for_platform(
                    ResumeContext {
                        bridge_executable: Path::new("/opt/agent-bridge"),
                        directory: &session,
                        provider_session_id: REOPEN_TEST_CONVERSATION,
                        yolo,
                        model,
                        effort,
                    },
                    true,
                )
                .unwrap();
                let label = format!("yolo={yolo} model={model:?} effort={effort:?}");
                let mut expected = claude_policy(yolo, mapped, effort);
                expected.extend([
                    OsString::from("--resume"),
                    OsString::from(REOPEN_TEST_CONVERSATION),
                    OsString::from("--settings"),
                    settings_path.clone().into_os_string(),
                    OsString::from("--name"),
                    OsString::from("session-reopenplan"),
                ]);
                assert_eq!(plan.arguments, expected, "{label}");
                assert_eq!(
                    plan.initial_prompt,
                    InitialPromptDisposition::AwaitingDelivery,
                    "{label}"
                );
                assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
            }
        }
    }
}
