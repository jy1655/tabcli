#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge::FirstPartyCli;

    #[test]
    fn ask_yolo_is_false_unless_the_child_request_contains_the_flag() {
        let command = parse_args([
            "ask",
            "codex",
            "--workspace",
            "/tmp/project",
            "--prompt",
            "review this",
        ])
        .unwrap();

        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                provider: FirstPartyCli::Codex,
                workspace,
                yolo: false,
                ..
            }) if workspace.as_path() == Path::new("/tmp/project")
        ));
    }

    #[test]
    fn ask_model_is_supported_by_every_native_provider() {
        for provider in ["codex", "claude", "agy", "pi"] {
            let command = parse_args([
                "ask",
                provider,
                "--prompt",
                "review this",
                "--model",
                "provider-model",
            ])
            .unwrap_or_else(|error| panic!("{provider} rejected --model: {error:#}"));

            assert!(matches!(
                command,
                NativeCommand::Ask(AskRequest {
                    model: Some(model),
                    ..
                }) if model == "provider-model"
            ));
        }
    }

    #[test]
    fn model_and_effort_are_not_inherited_when_omitted() {
        for provider in ["codex", "claude", "agy", "pi"] {
            let command = parse_args(["ask", provider, "--prompt", "review this"]).unwrap();
            assert!(matches!(
                command,
                NativeCommand::Ask(AskRequest {
                    model: None,
                    effort: None,
                    ..
                })
            ));
        }
    }

    #[test]
    fn ask_effort_is_supported_by_every_native_provider() {
        for (provider, requested_effort) in [
            ("codex", "xhigh"),
            ("claude", "max"),
            ("agy", "high"),
            ("pi", "minimal"),
        ] {
            let command = parse_args([
                "ask",
                provider,
                "--prompt",
                "review this",
                "--effort",
                requested_effort,
            ])
            .unwrap();

            assert!(matches!(
                command,
                NativeCommand::Ask(AskRequest {
                    effort: Some(effort),
                    ..
                }) if effort == requested_effort
            ));
        }
    }

    #[test]
    fn native_ask_and_tell_reject_unrepresentable_timeouts() {
        let too_large = u64::MAX.to_string();
        assert!(
            parse_args([
                "ask",
                "codex",
                "--prompt",
                "review this",
                "--timeout-secs",
                &too_large,
            ])
            .is_err()
        );
        assert!(
            parse_args([
                "tell",
                "session-safe123",
                "--prompt",
                "continue",
                "--timeout-secs",
                &too_large,
            ])
            .is_err()
        );
    }

    #[test]
    fn effort_uses_each_provider_native_session_option() {
        assert_eq!(
            provider_effort_args(FirstPartyCli::Codex, "xhigh").unwrap(),
            ["-c", "model_reasoning_effort=\"xhigh\""]
        );
        assert_eq!(
            provider_effort_args(FirstPartyCli::Claude, "max").unwrap(),
            ["--effort", "max"]
        );
        assert_eq!(
            provider_effort_args(FirstPartyCli::Agy, "high").unwrap(),
            ["--effort", "high"]
        );
        assert_eq!(
            provider_effort_args(FirstPartyCli::Pi, "minimal").unwrap(),
            ["--thinking", "minimal"]
        );
    }

    #[test]
    fn agy_log_and_transcript_parsers_accept_only_the_expected_completed_result() {
        let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
        assert_eq!(
            parse_agy_conversation_id(&format!("prefix Created conversation {id}\n")),
            Some(id.to_owned())
        );
        assert!(parse_agy_conversation_id("Created conversation ../../outside").is_none());

        let completed = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":9,"content":"AGY_TOOL_OK"}"#;
        assert_eq!(
            parse_agy_transcript_line(completed),
            Some((9, "AGY_TOOL_OK".to_owned()))
        );
        let intermediate = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":7,"content":""}"#;
        assert_eq!(parse_agy_transcript_line(intermediate), None);
        let planner_tool = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":8,"content":"checking","tool_calls":[{"name":"run_command"}]}"#;
        assert_eq!(parse_agy_transcript_line(planner_tool), None);
        let tool = r#"{"type":"RUN_COMMAND","status":"DONE","source":"MODEL","step_index":8,"content":"output"}"#;
        assert_eq!(parse_agy_transcript_line(tool), None);
    }

    #[test]
    fn pi_session_extension_reports_only_settled_results_without_changing_tool_policy() {
        let extension = pi_bridge_extension();

        assert!(extension.contains("agent_start"));
        assert!(extension.contains("agent_end"));
        assert!(extension.contains("agent_settled"));
        assert!(extension.contains("stopReason"));
        assert!(extension.contains("agent_bridge_error"));
        assert!(extension.contains("pi-hook-failure.json"));
        assert!(extension.contains("renameSync"));
        assert!(extension.contains("native-hook\", \"pi"));
        assert!(!extension.contains("tool_call"));
        assert!(!extension.contains("--approve"));
    }

    #[test]
    fn provider_failures_finish_the_bridge_turn_without_reporting_success() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();

        record_provider_failure(
            directory.path(),
            FirstPartyCli::Pi,
            "Pi turn aborted",
            Some("provider-session".to_owned()),
            Some("provider-turn".to_owned()),
        )
        .unwrap();

        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
        assert_eq!(status.error.as_deref(), Some("Pi turn aborted"));
        let error = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error:#}").contains("Pi turn aborted"));
    }

    #[test]
    fn pi_hook_transport_failure_signal_recovers_the_bridge_turn() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        write_json_atomic(
            &directory.path().join(PI_HOOK_FAILURE_FILE),
            &PiHookFailureSignal {
                error: "native hook exited with status 1".to_owned(),
                provider_session_id: Some("provider-session".to_owned()),
                turn_id: Some("provider-turn".to_owned()),
            },
        )
        .unwrap();

        assert!(consume_pi_hook_failure(directory.path()).unwrap());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(PI_HOOK_FAILURE_FILE).exists());
        let error = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error:#}").contains("native hook exited with status 1"));
    }

    #[test]
    fn close_is_rejected_without_the_explicit_flag() {
        assert!(parse_args(["close-session", "session-safe123"]).is_err());
        assert!(parse_args(["close-session", "session-safe123", "--explicit"]).is_ok());
    }

    #[test]
    fn explicit_close_repairs_failed_launch_without_terminal_record() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(
            directory.path(),
            "failed",
            None,
            Some("launch failed".to_owned()),
        )
        .unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        let mut close_was_called = false;

        close_session_state(directory.path(), |_| {
            close_was_called = true;
            Ok(ItermCloseOutcome::Closed)
        })
        .unwrap();

        assert!(!close_was_called);
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
    }

    #[test]
    fn explicit_close_is_idempotent_when_iterm_session_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        write_json_atomic(
            &directory.path().join("terminal.json"),
            &TerminalRecord {
                iterm_session_id: "missing-iterm-session".to_owned(),
            },
        )
        .unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        let mut close_calls = 0;

        for _ in 0..2 {
            close_session_state(directory.path(), |id| {
                assert_eq!(id, "missing-iterm-session");
                close_calls += 1;
                Ok(ItermCloseOutcome::Missing)
            })
            .unwrap();
        }

        assert_eq!(close_calls, 2);
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
    }

    #[test]
    fn explicit_close_is_terminal_against_late_native_wrapper_updates() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        write_json_atomic(
            &directory.path().join("terminal.json"),
            &TerminalRecord {
                iterm_session_id: "closing-iterm-session".to_owned(),
            },
        )
        .unwrap();
        update_status(directory.path(), "running", None, None).unwrap();

        close_session_state(directory.path(), |_| Ok(ItermCloseOutcome::Closed)).unwrap();
        update_status(directory.path(), "exited", Some(1), None).unwrap();
        update_status(
            directory.path(),
            "failed",
            None,
            Some("provider exited after close".to_owned()),
        )
        .unwrap();

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
        assert_eq!(status.exit_code, None);
        assert_eq!(status.error, None);
    }

    #[test]
    fn internal_session_ids_cannot_escape_the_state_root() {
        assert!(valid_session_id("session-abCD_123-xyz"));
        assert!(!valid_session_id("../outside"));
        assert!(!valid_session_id("session/child"));
    }

    #[test]
    fn hook_payload_extracts_first_party_assistant_results() {
        let codex = serde_json::json!({ "last-assistant-message": "codex result" });
        let claude = serde_json::json!({ "last_assistant_message": "claude result" });

        assert_eq!(extract_assistant_message(&codex), Some("codex result"));
        assert_eq!(extract_assistant_message(&claude), Some("claude result"));
    }

    #[test]
    fn iterm_script_keeps_dynamic_values_in_argv() {
        assert!(!terminal::macos_iterm::OPEN_TAB_SCRIPT.contains("review this"));
        assert!(terminal::macos_iterm::OPEN_TAB_SCRIPT.contains("item 1 of argv"));
        assert!(terminal::macos_iterm::OPEN_TAB_SCRIPT.contains("write text bridgeCommand"));
    }

    #[test]
    fn iterm_follow_up_sends_an_explicit_carriage_return() {
        assert!(terminal::macos_iterm::SEND_FILE_SCRIPT.contains("ASCII character 13"));
        assert!(terminal::macos_iterm::SEND_FILE_SCRIPT.contains("newline NO"));
        assert!(!terminal::macos_iterm::SEND_FILE_SCRIPT.contains("write text \"\""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn iterm_applescripts_compile_without_opening_a_tab() {
        for script in [
            terminal::macos_iterm::OPEN_TAB_SCRIPT,
            terminal::macos_iterm::SEND_FILE_SCRIPT,
            terminal::macos_iterm::CLOSE_SESSION_SCRIPT,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new("/usr/bin/osacompile")
                .arg("-o")
                .arg(directory.path().join("bridge.scpt"))
                .arg("-e")
                .arg(script)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "AppleScript did not compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn status_updates_replace_atomically() {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        update_status(directory.path(), "ready", None, None).unwrap();

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
    }

    #[test]
    fn claude_settings_capture_only_stop_for_the_native_session() {
        let settings =
            provider::claude_hook_settings(Path::new("/opt/Agent Bridge/bin/agent-bridge"));
        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();

        assert_eq!(
            command,
            "'/opt/Agent Bridge/bin/agent-bridge' native-hook claude"
        );
        assert!(settings["hooks"]["PermissionRequest"].is_null());
    }

    #[test]
    fn shell_quoting_handles_apostrophes_without_executing_them() {
        assert_eq!(
            shell_quote(std::ffi::OsStr::new("/tmp/user's bridge")),
            "'/tmp/user'\"'\"'s bridge'"
        );
    }

    #[test]
    fn bridge_shell_command_quotes_the_workspace_and_executable() {
        assert_eq!(
            bridge_shell_command(
                Path::new("/tmp/project; touch nope"),
                Path::new("/tmp/state root"),
                Path::new("/tmp/Agent Bridge/bin"),
                "session-safe123",
            )
            .unwrap(),
            "cd '/tmp/project; touch nope' && AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'"
        );
    }

    #[test]
    fn bridge_shell_command_rejects_controls_in_dynamic_components() {
        for control in ['\0', '\n', '\t', '\r', '\u{1b}', '\u{7f}'] {
            let unsafe_value = format!("unsafe{control}value");
            for result in [
                bridge_shell_command(
                    Path::new(&unsafe_value),
                    Path::new("/tmp/state"),
                    Path::new("/tmp/bridge"),
                    "session-safe123",
                ),
                bridge_shell_command(
                    Path::new("/tmp/workspace"),
                    Path::new(&unsafe_value),
                    Path::new("/tmp/bridge"),
                    "session-safe123",
                ),
                bridge_shell_command(
                    Path::new("/tmp/workspace"),
                    Path::new("/tmp/state"),
                    Path::new(&unsafe_value),
                    "session-safe123",
                ),
                bridge_shell_command(
                    Path::new("/tmp/workspace"),
                    Path::new("/tmp/state"),
                    Path::new("/tmp/bridge"),
                    &unsafe_value,
                ),
            ] {
                assert!(
                    result.is_err(),
                    "accepted terminal control U+{:04X}",
                    u32::from(control)
                );
            }
        }
    }

    #[test]
    fn follow_up_prompts_only_enter_a_completed_live_cli_turn() {
        assert!(session_accepts_prompt("ready"));
        for state in [
            "launching",
            "running",
            "working",
            "exited",
            "failed",
            "closed",
        ] {
            assert!(!session_accepts_prompt(state), "accepted {state}");
        }
    }

    #[test]
    fn follow_up_prompt_is_one_bracketed_paste_payload() {
        assert_eq!(
            terminal_paste_bytes("line one\nline two"),
            b"\x1b[200~line one\nline two\x1b[201~"
        );
    }

    #[test]
    fn native_turn_claim_is_exclusive_until_the_hook_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        assert!(acquire_turn_claim(directory.path()).is_err());

        claim.retain();
        assert!(acquire_turn_claim(directory.path()).is_err());
        release_turn_claim(directory.path()).unwrap();
        assert!(acquire_turn_claim(directory.path()).is_ok());
    }

    fn reaped_child_pid() -> u32 {
        #[cfg(windows)]
        let mut child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        #[cfg(not(windows))]
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn live_native_session_owner_is_not_repaired() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        write_json_atomic(
            &directory.path().join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: std::process::id(),
            },
        )
        .unwrap();

        assert!(!repair_dead_native_owner(directory.path()).unwrap());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "working");
    }

    #[test]
    fn dead_native_session_owner_releases_the_turn_and_closes_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        write_json_atomic(
            &directory.path().join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: reaped_child_pid(),
            },
        )
        .unwrap();

        assert!(repair_dead_native_owner(directory.path()).unwrap());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no longer running"))
        );
        assert!(directory.path().join("events").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn provider_resolution_rejects_relative_path_entries() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::Builder::new()
            .prefix("relative-provider-")
            .tempdir_in(&cwd)
            .unwrap();
        let relative = root.path().strip_prefix(&cwd).unwrap();
        let executable = root.path().join("codex");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(resolve_provider_from_path(FirstPartyCli::Codex, relative.as_os_str()).is_err());
    }

    #[test]
    fn every_native_prompt_carries_a_sanitized_source_provenance() {
        assert_eq!(
            native_delegation_prompt("Codex parent\nforged", "review this"),
            "[Agent Bridge native delegation]\nSource: Codex parent forged\n\nreview this"
        );
    }

    #[test]
    fn terminal_titles_drop_control_characters_and_are_bounded() {
        assert_eq!(sanitize_title(" Review\nTab\t ").unwrap(), "Review Tab");
        assert_eq!(
            sanitize_title(&"x".repeat(200)).unwrap().chars().count(),
            80
        );
        assert!(sanitize_title("\n\t").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_session_executes_the_provider_with_policy_and_provenance() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let provider = root.path().join("fake-codex");
        fs::write(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'codex-cli 0.147.0\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\npwd > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/cwd.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-safe123".to_owned(),
                provider: "codex".to_owned(),
                provider_path: provider,
                provider_version: "codex-cli 0.147.0".to_owned(),
                workspace: workspace.clone(),
                title: "Codex test".to_owned(),
                model: Some("gpt-daybreak-blue-latest".to_owned()),
                effort: Some("xhigh".to_owned()),
                yolo: true,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
        write_private(
            &directory.join("initial-prompt.txt"),
            native_delegation_prompt("parent", "review this").as_bytes(),
        )
        .unwrap();

        run_session_inner(&directory).unwrap();

        let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
        assert!(arguments.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(arguments.contains("--model\ngpt-daybreak-blue-latest"));
        assert!(arguments.contains("-c\nmodel_reasoning_effort=\"xhigh\""));
        assert!(arguments.contains("notify=["));
        assert!(arguments.contains("native-hook"));
        assert!(arguments.contains("[Agent Bridge native delegation]"));
        assert!(arguments.contains("Source: parent"));
        assert!(!directory.join("initial-prompt.txt").exists());
        assert_eq!(
            fs::read_to_string(directory.join("cwd.txt"))
                .unwrap()
                .trim(),
            workspace.canonicalize().unwrap().to_string_lossy()
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "exited");
    }

    #[cfg(unix)]
    #[test]
    fn claude_session_forwards_requested_model() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let provider = root.path().join("fake-claude");
        fs::write(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '2.1.229 (Claude Code)\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-safe123".to_owned(),
                provider: "claude".to_owned(),
                provider_path: provider,
                provider_version: "2.1.229 (Claude Code)".to_owned(),
                workspace,
                title: "Claude test".to_owned(),
                model: Some("Fable5".to_owned()),
                effort: Some("high".to_owned()),
                yolo: false,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
        write_private(&directory.join("initial-prompt.txt"), b"claude prompt").unwrap();

        run_session_inner(&directory).unwrap();

        let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
        assert!(arguments.contains("--model\nFable"));
        assert!(!arguments.contains("Fable5"));
        assert!(arguments.contains("--effort\nhigh"));
        assert!(arguments.contains("--settings"));
        assert!(arguments.ends_with("claude prompt\n"));
    }

    #[test]
    fn agy_transcript_cursor_records_each_completed_response_once() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, "working", None, None).unwrap();

        let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
        let brain = root.path().join("brain");
        let transcript_path = brain
            .join(id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":1,\"content\":\"first\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":2,\"content\":\"still working\",\"tool_calls\":[{\"name\":\"run_command\"}]}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":3,\"content\":\"short...\",\"is_truncated\":true}\n"
            ),
        )
        .unwrap();
        let mut cursor = AgyTranscriptCursor::new(transcript_path.clone());
        cursor.poll(&directory, &brain, id).unwrap();
        cursor.poll(&directory, &brain, id).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);

        fs::write(
            transcript_path.with_file_name("transcript_full.jsonl"),
            concat!(
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":1,\"content\":\"first\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":2,\"content\":\"still working\",\"tool_calls\":[{\"name\":\"run_command\"}]}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":3,\"content\":\"complete long response\"}\n"
            ),
        )
        .unwrap();
        cursor.poll(&directory, &brain, id).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!(latest.message, "complete long response");

        let mut transcript = OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        writeln!(
            transcript,
            "{{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":4,\"content\":\"second\"}}"
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
        assert_eq!(status.state, "ready");
    }

    #[test]
    fn agy_monitor_switches_to_the_newest_created_conversation() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, "working", None, None).unwrap();
        let brain = root.path().join("brain");
        let log = directory.join("agy.log");
        let first_id = "11111111-1111-1111-1111-111111111111";
        let second_id = "22222222-2222-2222-2222-222222222222";
        for (id, message) in [(first_id, "before clear"), (second_id, "after clear")] {
            let transcript = brain
                .join(id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(
                transcript,
                format!(
                    "{{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"source\":\"MODEL\",\"step_index\":1,\"content\":{}}}\n",
                    serde_json::to_string(message).unwrap()
                ),
            )
            .unwrap();
        }
        fs::write(&log, format!("Created conversation {first_id}\n")).unwrap();
        let mut monitor = AgyMonitorState::default();

        monitor.poll(&directory, &log, &brain).unwrap();
        update_status(&directory, "working", None, None).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn agy_session_uses_interactive_prompt_model_log_and_explicit_yolo() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let provider = root.path().join("fake-agy");
        fs::write(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '1.1.12\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-safe123".to_owned(),
                provider: "agy".to_owned(),
                provider_path: provider,
                provider_version: "1.1.12".to_owned(),
                workspace,
                title: "Agy test".to_owned(),
                model: Some("gemini-model".to_owned()),
                effort: Some("high".to_owned()),
                yolo: true,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
        write_private(&directory.join("initial-prompt.txt"), b"agy prompt").unwrap();

        run_session_inner(&directory).unwrap();

        let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
        assert!(arguments.contains("--dangerously-skip-permissions"));
        assert!(arguments.contains("--model\ngemini-model"));
        assert!(arguments.contains("--effort\nhigh"));
        assert!(arguments.contains("--log-file"));
        assert!(arguments.contains(directory.join("agy.log").to_string_lossy().as_ref()));
        assert!(arguments.contains("--prompt-interactive\nagy prompt"));
    }

    #[cfg(unix)]
    #[test]
    fn pi_session_loads_only_the_result_extension_and_preserves_native_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let provider = root.path().join("fake-pi");
        fs::write(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '0.84.1\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        write_json_atomic(
            &directory.join("manifest.json"),
            &SessionManifest {
                schema: SESSION_SCHEMA,
                id: "session-safe123".to_owned(),
                provider: "pi".to_owned(),
                provider_path: provider,
                provider_version: "0.84.1".to_owned(),
                workspace,
                title: "Pi test".to_owned(),
                model: Some("provider/model".to_owned()),
                effort: Some("minimal".to_owned()),
                yolo: true,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
        write_private(&directory.join("initial-prompt.txt"), b"pi prompt").unwrap();

        run_session_inner(&directory).unwrap();

        let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
        assert!(arguments.contains("--model\nprovider/model"));
        assert!(arguments.contains("--thinking\nminimal"));
        assert!(arguments.contains("--extension"));
        assert!(arguments.contains("--name\nPi test"));
        assert!(arguments.ends_with("pi prompt\n"));
        assert!(!arguments.contains("--approve"));
        assert!(!arguments.contains("dangerously"));
        let extension = fs::read_to_string(directory.join("pi-agent-bridge.js")).unwrap();
        assert_eq!(extension, pi_bridge_extension());
    }
}
mod provider;
mod terminal;

use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agent_bridge::{
    FirstPartyCli, checked_deadline_from, cli_version_is_supported, confirm_explicit_close,
    process_is_alive, provider_effort_args, provider_launch_args, provider_model_args,
    terminal_safe_text, validate_terminal_input,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const STATE_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_STATE_DIR";
const SESSION_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_SESSION_DIR";
const DEFAULT_TIMEOUT_SECS: u64 = 900;
const SESSION_SCHEMA: u32 = 1;
const TURN_CLAIM_FILE: &str = "turn.claim";
const SESSION_OWNER_FILE: &str = "native-session.json";
const CLOSED_STATUS_FILE: &str = "closed.json";
const PI_HOOK_FAILURE_FILE: &str = "pi-hook-failure.json";

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Ask(AskRequest),
    Tell(TellRequest),
    Sessions {
        json: bool,
    },
    Close(CloseRequest),
    RunSession {
        id: String,
    },
    Hook {
        provider: FirstPartyCli,
        payload: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct AskRequest {
    pub(crate) provider: FirstPartyCli,
    pub(crate) workspace: PathBuf,
    pub(crate) prompt: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) yolo: bool,
    pub(crate) timeout: Duration,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

#[derive(Debug)]
pub(crate) struct TellRequest {
    id: String,
    prompt: String,
    timeout: Duration,
    detach: bool,
    json: bool,
}

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionManifest {
    schema: u32,
    id: String,
    provider: String,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    yolo: bool,
    created_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
struct TerminalRecord {
    iterm_session_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItermCloseOutcome {
    Closed,
    Missing,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionStatus {
    state: String,
    updated_unix_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionEvent {
    provider: String,
    message: String,
    #[serde(default)]
    error: Option<String>,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    created_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
struct PiHookFailureSignal {
    error: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct NativeSessionOwner {
    pid: u32,
}

struct CreatedSession {
    id: String,
    directory: PathBuf,
    manifest: SessionManifest,
}

struct SessionSpec {
    provider: FirstPartyCli,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    model: Option<String>,
    effort: Option<String>,
    yolo: bool,
    prompt: String,
}

pub(crate) fn is_command(value: &str) -> bool {
    matches!(
        value,
        "ask" | "tell" | "sessions" | "close-session" | "native-session" | "native-hook"
    )
}

pub(crate) fn parse_args<I, S>(args: I) -> Result<NativeCommand>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    let (command, rest) = args.split_first().context("native command is required")?;
    match command.as_str() {
        "ask" => parse_ask(rest),
        "tell" => parse_tell(rest),
        "sessions" => parse_sessions(rest),
        "close-session" => parse_close(rest),
        "native-session" => {
            let id = one_positional(rest, "native-session requires one session id")?;
            require_valid_session_id(id)?;
            Ok(NativeCommand::RunSession { id: id.to_owned() })
        }
        "native-hook" => parse_hook(rest),
        _ => bail!("unknown native command: {command}"),
    }
}

fn parse_ask(args: &[String]) -> Result<NativeCommand> {
    let (provider, options) = args
        .split_first()
        .context("ask requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    let mut workspace = None;
    let mut prompt = None;
    let mut title = None;
    let mut model = None;
    let mut effort = None;
    let mut yolo = false;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--title" => set_once(
                &mut title,
                option_value(options, &mut index, "--title")?.to_owned(),
                "--title",
            )?,
            "--model" => set_once(
                &mut model,
                option_value(options, &mut index, "--model")?.to_owned(),
                "--model",
            )?,
            "--effort" => set_once(
                &mut effort,
                option_value(options, &mut index, "--effort")?.to_owned(),
                "--effort",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--yolo" => set_flag_once(&mut yolo, "--yolo")?,
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    let prompt = prompt.context("ask requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    if model.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--model cannot be empty");
    }
    if effort.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--effort cannot be empty");
    }
    Ok(NativeCommand::Ask(AskRequest {
        provider,
        workspace,
        prompt,
        title,
        model,
        effort,
        yolo,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_tell(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args.split_first().context("tell requires one session id")?;
    require_valid_session_id(id)?;
    let mut prompt = None;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown tell option: {option}"),
        }
        index += 1;
    }
    let prompt = prompt.context("tell requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
    Ok(NativeCommand::Tell(TellRequest {
        id: id.to_owned(),
        prompt,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_sessions(args: &[String]) -> Result<NativeCommand> {
    match args {
        [] => Ok(NativeCommand::Sessions { json: false }),
        [option] if option == "--json" => Ok(NativeCommand::Sessions { json: true }),
        _ => bail!("sessions accepts only --json"),
    }
}

fn parse_close(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("close-session requires one session id")?;
    require_valid_session_id(id)?;
    let mut explicit = false;
    let mut json = false;
    for option in options {
        match option.as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            _ => bail!("unknown close-session option: {option}"),
        }
    }
    confirm_explicit_close(explicit)?;
    Ok(NativeCommand::Close(CloseRequest {
        id: id.to_owned(),
        explicit,
        json,
    }))
}

fn parse_hook(args: &[String]) -> Result<NativeCommand> {
    let (provider, payload) = args
        .split_first()
        .context("native-hook requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    if payload.len() > 1 {
        bail!("native-hook accepts at most one payload argument");
    }
    Ok(NativeCommand::Hook {
        provider,
        payload: payload.first().cloned(),
    })
}

fn one_positional<'a>(args: &'a [String], message: &str) -> Result<&'a str> {
    match args {
        [value] => Ok(value),
        _ => bail!("{message}"),
    }
}

fn option_value<'a>(args: &'a [String], index: &mut usize, option: &str) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{option} may only be specified once");
    }
    Ok(())
}

fn set_flag_once(flag: &mut bool, option: &str) -> Result<()> {
    if *flag {
        bail!("{option} may only be specified once");
    }
    *flag = true;
    Ok(())
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("invalid timeout: {value}"))?;
    if seconds == 0 {
        bail!("timeout must be greater than zero");
    }
    let timeout = Duration::from_secs(seconds);
    checked_deadline_from(Instant::now(), timeout)?;
    Ok(timeout)
}

pub(crate) fn valid_session_id(value: &str) -> bool {
    value.starts_with("session-")
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn require_valid_session_id(value: &str) -> Result<()> {
    if !valid_session_id(value) {
        bail!("invalid Agent Bridge session id: {value:?}");
    }
    Ok(())
}

pub(crate) fn run(command: NativeCommand) -> Result<()> {
    match command {
        NativeCommand::Ask(request) => run_ask(request),
        NativeCommand::Tell(request) => run_tell(request),
        NativeCommand::Sessions { json } => run_sessions(json),
        NativeCommand::Close(request) => run_close(request),
        NativeCommand::RunSession { id } => run_session(&id),
        NativeCommand::Hook { provider, payload } => run_hook(provider, payload.as_deref()),
    }
}

fn run_ask(request: AskRequest) -> Result<()> {
    terminal::macos_iterm::ensure_available()?;
    let workspace = request.workspace.canonicalize().with_context(|| {
        format!(
            "workspace does not exist or cannot be resolved: {}",
            request.workspace.display()
        )
    })?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let provider_path = resolve_provider(request.provider)?;
    let provider_version = check_provider_version(request.provider, &provider_path)?;
    let requested_title = request.title.unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        format!("{} · {workspace_name}", request.provider.as_str())
    });
    let title = sanitize_title(&requested_title)?;
    let created = create_session(SessionSpec {
        provider: request.provider,
        provider_path,
        provider_version,
        workspace,
        title,
        model: request.model,
        effort: request.effort,
        yolo: request.yolo,
        prompt: native_delegation_prompt(&delegation_source(), &request.prompt),
    })?;
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let bridge_command = bridge_shell_command(
        &created.manifest.workspace,
        created
            .directory
            .parent()
            .context("session directory has no state root")?,
        &executable,
        &created.id,
    )?;
    let iterm_session_id =
        match terminal::macos_iterm::open_tab(&bridge_command, &created.manifest.title) {
            Ok(id) => id,
            Err(error) => {
                let _ = fs::remove_file(created.directory.join("initial-prompt.txt"));
                let _ = update_status(
                    &created.directory,
                    "failed",
                    None,
                    Some(format!("{error:#}")),
                );
                return Err(error).with_context(|| {
                    format!("failed to open iTerm tab for session {}", created.id)
                });
            }
        };
    write_json_atomic(
        &created.directory.join("terminal.json"),
        &TerminalRecord {
            iterm_session_id: iterm_session_id.clone(),
        },
    )?;

    if request.detach {
        return emit_session_result(
            request.json,
            &created.id,
            &iterm_session_id,
            request.provider,
            None,
        );
    }

    let event = wait_for_event(&created.directory, 0, request.timeout).with_context(|| {
        format!(
            "session {} remains open in iTerm; use `agent-bridge sessions` to inspect it",
            created.id
        )
    })?;
    emit_session_result(
        request.json,
        &created.id,
        &iterm_session_id,
        request.provider,
        Some(&event),
    )
}

fn run_tell(request: TellRequest) -> Result<()> {
    terminal::macos_iterm::ensure_available()?;
    let directory = session_directory(&request.id)?;
    repair_dead_native_owner(&directory)?;
    let claim = acquire_turn_claim(&directory)?;
    let manifest = read_manifest(&directory)?;
    let terminal: TerminalRecord = read_json(&directory.join("terminal.json"))?;
    let baseline = event_paths(&directory)?.len();
    let previous_state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&previous_state) {
        bail!(
            "session {} is {previous_state}; tell requires the ready state",
            request.id
        );
    }
    update_status(&directory, "working", None, None)?;

    let mut prompt_file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(&directory)?;
    set_private_file_permissions(prompt_file.as_file())?;
    let prompt = native_delegation_prompt(&delegation_source(), &request.prompt);
    prompt_file.write_all(&terminal_paste_bytes(&prompt))?;
    prompt_file.flush()?;
    if let Err(error) =
        terminal::macos_iterm::send_file(&terminal.iterm_session_id, prompt_file.path())
    {
        let _ = update_status(
            &directory,
            &previous_state,
            None,
            Some(format!("{error:#}")),
        );
        return Err(error)
            .with_context(|| format!("failed to type into visible iTerm session {}", request.id));
    }
    claim.retain();

    if request.detach {
        return emit_session_result(
            request.json,
            &request.id,
            &terminal.iterm_session_id,
            FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
            None,
        );
    }
    let event = wait_for_event(&directory, baseline, request.timeout).with_context(|| {
        format!(
            "session {} remains open in iTerm; the requested turn did not report completion",
            request.id
        )
    })?;
    emit_session_result(
        request.json,
        &request.id,
        &terminal.iterm_session_id,
        FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
        Some(&event),
    )
}

fn run_sessions(json: bool) -> Result<()> {
    let root = state_root()?;
    let mut sessions = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_session_id(&id) {
                continue;
            }
            let directory = entry.path();
            let _ = repair_dead_native_owner(&directory);
            let Ok(manifest) = read_manifest(&directory) else {
                continue;
            };
            let status = read_json::<SessionStatus>(&directory.join("status.json")).ok();
            let terminal = read_json::<TerminalRecord>(&directory.join("terminal.json")).ok();
            sessions.push(serde_json::json!({
                "id": manifest.id,
                "provider": manifest.provider,
                "workspace": manifest.workspace,
                "title": manifest.title,
                "yolo": manifest.yolo,
                "state": status.as_ref().map(|value| value.state.as_str()).unwrap_or("unknown"),
                "iterm_session_id": terminal.map(|value| value.iterm_session_id),
                "results": event_paths(&directory).map(|paths| paths.len()).unwrap_or(0),
            }));
        }
    }
    sessions.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else if sessions.is_empty() {
        println!("no native Agent Bridge sessions");
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{}\t{}\tyolo={}\t{} result(s)",
                session["id"].as_str().unwrap_or("?"),
                terminal_safe_text(session["state"].as_str().unwrap_or("unknown"), false),
                terminal_safe_text(session["provider"].as_str().unwrap_or("?"), false),
                terminal_safe_text(session["workspace"].as_str().unwrap_or("?"), false),
                session["yolo"].as_bool().unwrap_or(false),
                session["results"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn run_close(request: CloseRequest) -> Result<()> {
    terminal::macos_iterm::ensure_available()?;
    confirm_explicit_close(request.explicit)?;
    let directory = session_directory(&request.id)?;
    close_session_state(&directory, terminal::macos_iterm::close_session)
        .with_context(|| format!("failed to close visible iTerm session {}", request.id))?;
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": request.id,
                "closed": true,
            }))?
        );
    } else {
        println!("closed {}", request.id);
    }
    Ok(())
}

fn close_session_state<F>(directory: &Path, mut close_terminal: F) -> Result<()>
where
    F: FnMut(&str) -> Result<ItermCloseOutcome>,
{
    let terminal_path = directory.join("terminal.json");
    let terminal = match fs::read_to_string(&terminal_path) {
        Ok(text) => Some(
            serde_json::from_str::<TerminalRecord>(&text)
                .with_context(|| format!("invalid JSON in {}", terminal_path.display()))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", terminal_path.display()));
        }
    };
    if let Some(terminal) = terminal {
        close_terminal(&terminal.iterm_session_id)?;
    }
    mark_session_closed(directory, None)
}

fn emit_session_result(
    json: bool,
    id: &str,
    iterm_session_id: &str,
    provider: FirstPartyCli,
    event: Option<&SessionEvent>,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": id,
                "provider": provider.as_str(),
                "iterm_session_id": iterm_session_id,
                "result": event.map(|value| value.message.as_str()),
                "provider_session_id": event.and_then(|value| value.provider_session_id.as_deref()),
                "turn_id": event.and_then(|value| value.turn_id.as_deref()),
            }))?
        );
    } else {
        println!("session: {id}");
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
    }
    Ok(())
}

fn run_session(id: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("native-session must run in a visible interactive terminal");
    }
    let directory = session_directory(id)?;
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: std::process::id(),
        },
    )?;
    let result = run_session_inner(&directory);
    let _ = release_turn_claim(&directory);
    if let Err(error) = &result {
        let _ = update_status(&directory, "failed", None, Some(format!("{error:#}")));
    }
    result
}

fn run_session_inner(directory: &Path) -> Result<()> {
    let manifest = read_manifest(directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = directory.join("initial-prompt.txt");
    let prompt = fs::read_to_string(&prompt_path).context("failed to read initial prompt")?;
    fs::remove_file(&prompt_path).context("failed to remove consumed initial prompt")?;

    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut arguments = provider_launch_args(provider, manifest.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = &manifest.effort {
        arguments.extend(
            provider_effort_args(provider, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = &manifest.model {
        arguments.extend(
            provider_model_args(provider, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    let provider::LaunchPlan {
        arguments: provider_arguments,
        prompt_is_positional,
        completion_monitor,
    } = provider::prepare_launch(
        provider,
        provider::LaunchContext {
            bridge_executable: &executable,
            directory,
            workspace: &manifest.workspace,
            title: &manifest.title,
            prompt: &prompt,
        },
    )?;
    arguments.extend(provider_arguments);
    if prompt_is_positional {
        arguments.push(OsString::from(prompt));
    }

    update_status(directory, "running", None, None)?;
    let (agy_monitor, pi_failure_monitor) = match completion_monitor {
        provider::CompletionMonitor::Hook => (None, None),
        provider::CompletionMonitor::AgyTranscript { log_path } => (
            Some(AgyMonitor::start(directory, &log_path, &agy_brain_root()?)?),
            None,
        ),
        provider::CompletionMonitor::PiHookFailure => {
            (None, Some(PiFailureMonitor::start(directory)?))
        }
    };
    let status = Command::new(&manifest.provider_path)
        .args(arguments)
        .current_dir(&manifest.workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", &manifest.id)
        .env("AGENT_BRIDGE_EXECUTABLE", &executable)
        .status();
    if let Some(monitor) = agy_monitor {
        monitor.stop()?;
    }
    if let Some(monitor) = pi_failure_monitor {
        monitor.stop()?;
    }
    let status = status.with_context(|| {
        format!(
            "failed to start {} at {}",
            provider.as_str(),
            manifest.provider_path.display()
        )
    })?;
    update_status(directory, "exited", status.code(), None)?;
    if !status.success() {
        bail!("{} exited with {status}", provider.as_str());
    }
    Ok(())
}

fn run_hook(provider: FirstPartyCli, argument_payload: Option<&str>) -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os(SESSION_DIR_ENV).context("native hook session directory is not set")?,
    );
    validate_hook_directory(&directory)?;
    let manifest = read_manifest(&directory)?;
    if manifest.provider != provider.as_str() {
        bail!(
            "hook provider {} does not match session provider {}",
            provider.as_str(),
            manifest.provider
        );
    }

    let payload_text = if let Some(payload) = argument_payload {
        payload.to_owned()
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    let payload: serde_json::Value =
        serde_json::from_str(&payload_text).context("native hook received invalid JSON")?;
    let provider_session_id = json_string(&payload, &["session_id", "thread-id", "thread_id"]);
    let turn_id = json_string(&payload, &["turn_id", "turn-id"]);
    if let Some(error) = payload
        .get("agent_bridge_error")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return record_provider_failure(&directory, provider, error, provider_session_id, turn_id);
    }
    let message = extract_assistant_message(&payload)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("native hook payload did not contain an assistant result")?;
    record_provider_result(&directory, provider, message, provider_session_id, turn_id)
}

fn record_provider_result(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    write_event(directory, &event)?;
    update_status(directory, "ready", None, None)?;
    release_turn_claim(directory)
}

fn record_provider_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    write_event(directory, &event)?;
    update_status(directory, "ready", None, Some(error))?;
    release_turn_claim(directory)
}

pub(crate) fn extract_assistant_message(payload: &serde_json::Value) -> Option<&str> {
    ["last-assistant-message", "last_assistant_message"]
        .into_iter()
        .find_map(|key| payload.get(key).and_then(serde_json::Value::as_str))
}

fn json_string(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn pi_bridge_extension() -> &'static str {
    r#"import { spawnSync } from "node:child_process";
import { renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function assistantText(message) {
  if (!Array.isArray(message.content)) return undefined;
  const text = message.content
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim();
  return text || undefined;
}

function lastAssistantOutcome(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "stop") {
      const detail = typeof message.errorMessage === "string" && message.errorMessage.trim()
        ? `: ${message.errorMessage.trim()}`
        : "";
      return { agent_bridge_error: `Pi turn ended with ${message.stopReason ?? "an unknown state"}${detail}` };
    }
    const text = assistantText(message);
    if (text) return { last_assistant_message: text };
    return { agent_bridge_error: "Pi settled without assistant text." };
  }
  return { agent_bridge_error: "Pi settled without an assistant result." };
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function deliverResult(executable, payload) {
  if (!executable) {
    return { ok: false, detail: "Agent Bridge executable is unavailable" };
  }
  let detail = "native hook failed";
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const result = spawnSync(executable, ["native-hook", "pi"], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "pipe"],
    });
    if (!result.error && result.status === 0) return { ok: true };
    const stderr = typeof result.stderr === "string" ? result.stderr.trim() : "";
    detail = result.error?.message || stderr || `native hook exited with status ${result.status}`;
    if (attempt < 2) await wait(100 * (attempt + 1));
  }
  return { ok: false, detail: detail.slice(0, 1024) };
}

function persistHookFailure(payload, detail) {
  const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
  if (!directory) return false;
  const target = join(directory, "pi-hook-failure.json");
  const temporary = join(
    directory,
    `.pi-hook-failure-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`,
  );
  const signal = {
    error: `Pi result delivery failed: ${detail}`,
    provider_session_id: payload.session_id,
    turn_id: payload.turn_id,
  };
  try {
    writeFileSync(temporary, JSON.stringify(signal), { encoding: "utf8", flag: "wx", mode: 0o600 });
    renameSync(temporary, target);
    return true;
  } catch {
    try { unlinkSync(temporary); } catch {}
    return false;
  }
}

export default function (pi) {
  let pending;
  let undelivered = false;

  pi.on("agent_start", (_event, ctx) => {
    if (undelivered && pending) {
      if (!persistHookFailure(pending, "a prior result remained undelivered")) {
        ctx.ui.notify("Agent Bridge still cannot recover the previous Pi result.", "warning");
        return;
      }
      undelivered = false;
    }
    pending = undefined;
  });

  pi.on("agent_end", (event, ctx) => {
    if (undelivered) return;
    pending = {
      ...lastAssistantOutcome(event.messages),
      session_id: ctx.sessionManager.getSessionId(),
      turn_id: ctx.sessionManager.getLeafId() ?? undefined,
    };
  });

  pi.on("agent_settled", async (_event, ctx) => {
    if (!pending) return;
    const payload = pending;
    const executable = process.env.AGENT_BRIDGE_EXECUTABLE;
    const delivery = await deliverResult(executable, payload);
    if (!delivery.ok) {
      if (persistHookFailure(payload, delivery.detail)) {
        pending = undefined;
        ctx.ui.notify("Agent Bridge marked this undelivered Pi result as failed.", "warning");
        return;
      }
      undelivered = true;
      ctx.ui.notify("Agent Bridge could not record or recover this Pi result.", "warning");
      return;
    }
    pending = undefined;
  });
}
"#
}

struct PiFailureMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

impl PiFailureMonitor {
    fn start(directory: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-pi-failure-monitor".to_owned())
            .spawn(move || {
                let result = monitor_pi_hook_failures(&directory, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = update_status(
                        &error_directory,
                        "failed",
                        None,
                        Some(format!("Pi result recovery monitor failed: {error:#}")),
                    );
                    let _ = release_turn_claim(&error_directory);
                }
                result
            })
            .context("failed to start Pi result recovery monitor")?;
        Ok(Self { stop, handle })
    }

    fn stop(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("Pi result recovery monitor panicked"))??;
        Ok(())
    }
}

fn monitor_pi_hook_failures(directory: &Path, stop: &AtomicBool) -> Result<()> {
    loop {
        consume_pi_hook_failure(directory)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn consume_pi_hook_failure(directory: &Path) -> Result<bool> {
    let path = directory.join(PI_HOOK_FAILURE_FILE);
    let Some(text) = read_regular_text_if_present(&path)? else {
        return Ok(false);
    };
    let signal: PiHookFailureSignal =
        serde_json::from_str(&text).context("invalid Pi hook failure recovery signal")?;
    let error = signal.error.trim();
    if error.is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    fs::remove_file(&path).context("failed to consume Pi hook failure recovery signal")?;
    record_provider_failure(
        directory,
        FirstPartyCli::Pi,
        error,
        signal.provider_session_id,
        signal.turn_id,
    )?;
    Ok(true)
}

struct AgyMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

impl AgyMonitor {
    fn start(directory: &Path, log_path: &Path, brain_root: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let log_path = log_path.to_owned();
        let brain_root = brain_root.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-agy-monitor".to_owned())
            .spawn(move || {
                let result =
                    monitor_agy_session(&directory, &log_path, &brain_root, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = update_status(
                        &error_directory,
                        "failed",
                        None,
                        Some(format!("Agy result monitor failed: {error:#}")),
                    );
                    let _ = release_turn_claim(&error_directory);
                }
                result
            })
            .context("failed to start Agy result monitor")?;
        Ok(Self { stop, handle })
    }

    fn stop(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("Agy result monitor panicked"))??;
        Ok(())
    }
}

struct AgyTranscriptCursor {
    path: PathBuf,
    full_path: PathBuf,
    offset: u64,
    partial_line: Vec<u8>,
    pending_results: VecDeque<AgyPlannerResult>,
    greatest_result_step: Option<u64>,
}

impl AgyTranscriptCursor {
    fn new(path: PathBuf) -> Self {
        let full_path = path.with_file_name("transcript_full.jsonl");
        Self {
            path,
            full_path,
            offset: 0,
            partial_line: Vec::new(),
            pending_results: VecDeque::new(),
            greatest_result_step: None,
        }
    }

    fn poll(&mut self, directory: &Path, brain_root: &Path, conversation_id: &str) -> Result<()> {
        let Some(metadata) = validated_agy_file_metadata(&self.path, brain_root)? else {
            return Ok(());
        };
        if metadata.len() < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.pending_results.clear();
        }

        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.partial_line.extend_from_slice(&bytes);

        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in self.partial_line.iter().enumerate() {
            if *byte == b'\n' {
                lines.push(String::from_utf8_lossy(&self.partial_line[start..index]).into_owned());
                start = index + 1;
            }
        }
        if start > 0 {
            self.partial_line.drain(..start);
        }

        for line in lines {
            let Some(result) = parse_agy_planner_result(&line) else {
                continue;
            };
            if self
                .greatest_result_step
                .is_some_and(|previous| result.step <= previous)
                || self
                    .pending_results
                    .iter()
                    .any(|pending| pending.step == result.step)
            {
                continue;
            }
            self.pending_results.push_back(result);
        }

        while let Some(result) = self.pending_results.front() {
            let message = if result.truncated {
                let Some(message) = read_agy_full_result(&self.full_path, brain_root, result.step)?
                else {
                    break;
                };
                message
            } else {
                result.message.clone()
            };
            let step = result.step;
            record_provider_result(
                directory,
                FirstPartyCli::Agy,
                &message,
                Some(conversation_id.to_owned()),
                Some(step.to_string()),
            )?;
            self.greatest_result_step = Some(step);
            self.pending_results.pop_front();
        }
        Ok(())
    }
}

fn validated_agy_file_metadata(path: &Path, brain_root: &Path) -> Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular Agy transcript: {}", path.display());
    }
    let canonical_root = brain_root
        .canonicalize()
        .context("Agy brain directory is unavailable")?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("Agy transcript cannot be resolved: {}", path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!(
            "refusing Agy transcript outside its data directory: {}",
            path.display()
        );
    }
    Ok(Some(metadata))
}

fn read_agy_full_result(path: &Path, brain_root: &Path, step: u64) -> Result<Option<String>> {
    if validated_agy_file_metadata(path, brain_root)?.is_none() {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read full Agy transcript: {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("step_index").and_then(serde_json::Value::as_u64) != Some(step) {
            continue;
        }
        let result = parse_agy_planner_value(&value)
            .context("full Agy transcript row did not contain a final response")?;
        if result.truncated {
            bail!("full Agy transcript unexpectedly marked step {step} as truncated");
        }
        return Ok(Some(result.message));
    }
    Ok(None)
}

#[derive(Default)]
struct AgyMonitorState {
    conversation_id: Option<String>,
    transcript: Option<AgyTranscriptCursor>,
}

impl AgyMonitorState {
    fn poll(&mut self, directory: &Path, log_path: &Path, brain_root: &Path) -> Result<()> {
        if let Some(log) = read_regular_text_if_present(log_path)?
            && let Some(newest_id) = parse_agy_conversation_id(&log)
            && self.conversation_id.as_deref() != Some(newest_id.as_str())
        {
            let path = brain_root
                .join(&newest_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            self.conversation_id = Some(newest_id);
            self.transcript = Some(AgyTranscriptCursor::new(path));
        }
        if let (Some(id), Some(cursor)) =
            (self.conversation_id.as_deref(), self.transcript.as_mut())
        {
            cursor.poll(directory, brain_root, id)?;
        }
        Ok(())
    }
}

fn monitor_agy_session(
    directory: &Path,
    log_path: &Path,
    brain_root: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let mut state = AgyMonitorState::default();
    loop {
        state.poll(directory, log_path, brain_root)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn read_regular_text_if_present(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn agy_brain_root() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    Ok(home.join(".gemini").join("antigravity-cli").join("brain"))
}

fn parse_agy_conversation_id(log: &str) -> Option<String> {
    log.lines().rev().find_map(|line| {
        let (_, suffix) = line.rsplit_once("Created conversation ")?;
        let candidate = suffix.split_whitespace().next()?;
        valid_uuid(candidate).then(|| candidate.to_owned())
    })
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Debug)]
struct AgyPlannerResult {
    step: u64,
    message: String,
    truncated: bool,
}

fn parse_agy_planner_result(line: &str) -> Option<AgyPlannerResult> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    parse_agy_planner_value(&value)
}

fn parse_agy_planner_value(value: &serde_json::Value) -> Option<AgyPlannerResult> {
    if value.get("type")?.as_str()? != "PLANNER_RESPONSE"
        || value.get("status")?.as_str()? != "DONE"
        || value.get("source")?.as_str()? != "MODEL"
    {
        return None;
    }
    if value
        .get("tool_calls")
        .is_some_and(|tool_calls| match tool_calls {
            serde_json::Value::Null => false,
            serde_json::Value::Array(calls) => !calls.is_empty(),
            _ => true,
        })
    {
        return None;
    }
    let step = value.get("step_index")?.as_u64()?;
    let message = value.get("content")?.as_str()?.trim();
    (!message.is_empty()).then(|| AgyPlannerResult {
        step,
        message: message.to_owned(),
        truncated: value
            .get("is_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
fn parse_agy_transcript_line(line: &str) -> Option<(u64, String)> {
    let result = parse_agy_planner_result(line)?;
    (!result.truncated).then_some((result.step, result.message))
}

fn create_session(spec: SessionSpec) -> Result<CreatedSession> {
    let root = state_root()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create state directory {}", root.display()))?;
    set_private_directory_permissions(&root)?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&root)?;
    let directory = temp.keep();
    set_private_directory_permissions(&directory)?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("session directory name is not UTF-8")?
        .to_owned();
    require_valid_session_id(&id)?;
    let events = directory.join("events");
    fs::create_dir(&events)?;
    set_private_directory_permissions(&events)?;
    let manifest = SessionManifest {
        schema: SESSION_SCHEMA,
        id: id.clone(),
        provider: spec.provider.as_str().to_owned(),
        provider_path: spec.provider_path,
        provider_version: spec.provider_version,
        workspace: spec.workspace,
        title: spec.title,
        model: spec.model,
        effort: spec.effort,
        yolo: spec.yolo,
        created_unix_ms: unix_ms(),
    };
    write_json_atomic(&directory.join("manifest.json"), &manifest)?;
    write_private(
        &directory.join("initial-prompt.txt"),
        spec.prompt.as_bytes(),
    )?;
    update_status(&directory, "launching", None, None)?;
    Ok(CreatedSession {
        id,
        directory,
        manifest,
    })
}

fn state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(root));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    Ok(home.join(".agent-bridge").join("native-sessions"))
}

fn session_directory(id: &str) -> Result<PathBuf> {
    require_valid_session_id(id)?;
    let directory = state_root()?.join(id);
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("no such Agent Bridge session: {id}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("refusing non-directory Agent Bridge session: {id}");
    }
    Ok(directory)
}

fn validate_hook_directory(directory: &Path) -> Result<()> {
    let root = state_root()?
        .canonicalize()
        .context("native state root is missing")?;
    let canonical = directory
        .canonicalize()
        .context("native hook session directory is missing")?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "refusing native hook directory outside state root: {}",
            directory.display()
        );
    }
    let id = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .context("native hook session id is not UTF-8")?;
    require_valid_session_id(id)
}

fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    let manifest: SessionManifest = read_json(&directory.join("manifest.json"))?;
    if manifest.schema != SESSION_SCHEMA {
        bail!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        );
    }
    let expected_id = directory.file_name().and_then(|name| name.to_str());
    if expected_id != Some(manifest.id.as_str()) {
        bail!("session manifest id does not match its directory");
    }
    Ok(manifest)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.flush()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn update_status(
    directory: &Path,
    state: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    let status = SessionStatus {
        state: state.to_owned(),
        updated_unix_ms: unix_ms(),
        exit_code,
        error,
    };
    let status_path = directory.join("status.json");
    let closed_path = directory.join(CLOSED_STATUS_FILE);
    if state == "closed" {
        write_json_atomic(&closed_path, &status)?;
        return write_json_atomic(&status_path, &status);
    }
    if let Some(closed) = read_status_if_present(&closed_path)? {
        return write_json_atomic(&status_path, &closed);
    }
    write_json_atomic(&status_path, &status)?;
    if let Some(closed) = read_status_if_present(&closed_path)? {
        write_json_atomic(&status_path, &closed)?;
    }
    Ok(())
}

fn read_status_if_present(path: &Path) -> Result<Option<SessionStatus>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

struct TurnClaim {
    path: PathBuf,
    retained: bool,
}

impl TurnClaim {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if !self.retained {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn acquire_turn_claim(directory: &Path) -> Result<TurnClaim> {
    let path = directory.join(TURN_CLAIM_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| "another tell request already owns this session turn")?;
    set_private_file_permissions(&file)?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(TurnClaim {
        path,
        retained: false,
    })
}

fn release_turn_claim(directory: &Path) -> Result<()> {
    match fs::remove_file(directory.join(TURN_CLAIM_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to release native turn claim"),
    }
}

fn mark_session_closed(directory: &Path, error: Option<String>) -> Result<()> {
    let status_result = update_status(directory, "closed", None, error);
    let claim_result = release_turn_claim(directory);
    status_result?;
    claim_result
}

fn repair_dead_native_owner(directory: &Path) -> Result<bool> {
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(
        status.state.as_str(),
        "launching" | "running" | "working" | "ready"
    ) {
        return Ok(false);
    }
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner = match fs::read_to_string(&owner_path) {
        Ok(text) => serde_json::from_str::<NativeSessionOwner>(&text)
            .with_context(|| format!("invalid JSON in {}", owner_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", owner_path.display()));
        }
    };
    if process_is_alive(owner.pid) {
        return Ok(false);
    }
    mark_session_closed(
        directory,
        Some(format!(
            "native session process {} is no longer running",
            owner.pid
        )),
    )?;
    Ok(true)
}

fn write_event(directory: &Path, event: &SessionEvent) -> Result<()> {
    let events = directory.join("events");
    let name = format!(
        "event-{}-{}.json",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    write_json_atomic(&events.join(name), event)?;
    write_json_atomic(&directory.join("latest.json"), event)
}

fn event_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let events = directory.join("events");
    let mut paths = Vec::new();
    if !events.is_dir() {
        return Ok(paths);
    }
    for entry in fs::read_dir(events)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("event-") && name.ends_with(".json"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn wait_for_event(directory: &Path, baseline: usize, timeout: Duration) -> Result<SessionEvent> {
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    loop {
        repair_dead_native_owner(directory)?;
        let paths = event_paths(directory)?;
        if paths.len() > baseline {
            let event: SessionEvent = read_json(paths.last().context("event path disappeared")?)?;
            if let Some(error) = event.error.as_deref() {
                bail!("{error}");
            }
            return Ok(event);
        }
        if let Ok(status) = read_json::<SessionStatus>(&directory.join("status.json"))
            && matches!(status.state.as_str(), "failed" | "exited" | "closed")
        {
            let reason = status
                .error
                .unwrap_or_else(|| format!("session entered state {}", status.state));
            bail!("{reason}");
        }
        if Instant::now() >= deadline {
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn resolve_provider(provider: FirstPartyCli) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_provider_from_path(provider, &path)
}

fn resolve_provider_from_path(provider: FirstPartyCli, path: &std::ffi::OsStr) -> Result<PathBuf> {
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(provider.command());
        if candidate.is_file() && is_executable(&candidate) {
            let canonical = candidate.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize provider path {}",
                    candidate.display()
                )
            })?;
            if canonical.is_file() && is_executable(&canonical) {
                return Ok(canonical);
            }
        }
    }
    bail!(
        "{} was not found on an absolute PATH entry",
        provider.command()
    )
}

fn check_provider_version(provider: FirstPartyCli, executable: &Path) -> Result<String> {
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to query {} --version", executable.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            executable.display(),
            output.status
        );
    }
    let mut version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        version = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    if !cli_version_is_supported(provider, &version)? {
        bail!(
            "{} is too old: found {version:?}, require >= {}",
            provider.as_str(),
            provider.minimum_version()
        );
    }
    Ok(version)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn shell_quote(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn bridge_shell_command(
    workspace: &Path,
    state_root: &Path,
    executable: &Path,
    id: &str,
) -> Result<String> {
    for (value, field) in [
        (workspace.as_os_str(), "workspace"),
        (state_root.as_os_str(), "state root"),
        (executable.as_os_str(), "Agent Bridge executable"),
        (std::ffi::OsStr::new(id), "session id"),
    ] {
        validate_shell_command_component(value, field)?;
    }
    Ok(format!(
        "cd {} && {}={} {} native-session {}",
        shell_quote(workspace.as_os_str()),
        STATE_DIR_ENV,
        shell_quote(state_root.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(OsString::from(id).as_os_str())
    ))
}

fn validate_shell_command_component(value: &std::ffi::OsStr, field: &str) -> Result<()> {
    if let Some(character) = value
        .to_string_lossy()
        .chars()
        .find(|value| value.is_control())
    {
        bail!(
            "{field} contains terminal control U+{:04X}",
            u32::from(character)
        );
    }
    Ok(())
}

fn session_accepts_prompt(state: &str) -> bool {
    state == "ready"
}

fn delegation_source() -> String {
    std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID").unwrap_or_else(|_| "external".to_owned())
}

fn native_delegation_prompt(source: &str, prompt: &str) -> String {
    let source = source
        .chars()
        .take(128)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let source = if source.trim().is_empty() {
        "external"
    } else {
        source.trim()
    };
    format!(
        "[Agent Bridge native delegation]\nSource: {}\n\n{}",
        source,
        prompt.trim()
    )
}

fn terminal_paste_bytes(prompt: &str) -> Vec<u8> {
    format!("\x1b[200~{prompt}\x1b[201~").into_bytes()
}

fn sanitize_title(value: &str) -> Result<String> {
    let title = value
        .chars()
        .take(80)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        bail!("--title cannot be empty");
    }
    Ok(title)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}
