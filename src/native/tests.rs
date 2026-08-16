use super::*;
use agent_bridge::FirstPartyCli;
use std::process::Command;

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
fn ask_terminal_can_be_selected_without_changing_the_auto_default() {
    let automatic = parse_args(["ask", "pi", "--prompt", "review this"]).unwrap();
    assert!(matches!(
        automatic,
        NativeCommand::Ask(AskRequest { terminal: None, .. })
    ));

    for (requested, expected) in [
        ("ghostty", terminal::TerminalKind::Ghostty),
        ("iterm2", terminal::TerminalKind::Iterm2),
        ("terminal", terminal::TerminalKind::AppleTerminal),
    ] {
        let command = parse_args([
            "ask",
            "pi",
            "--prompt",
            "review this",
            "--terminal",
            requested,
        ])
        .unwrap();
        assert!(matches!(
            command,
            NativeCommand::Ask(AskRequest {
                terminal: Some(actual),
                ..
            }) if actual == expected
        ));
    }

    assert!(
        parse_args([
            "ask",
            "pi",
            "--prompt",
            "review this",
            "--terminal",
            "vscode",
        ])
        .is_err()
    );
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
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();

    assert!(!close_was_called);
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
}

#[test]
fn explicit_close_consumes_the_handle_and_repeated_close_skips_the_adapter() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "missing-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();
    let mut close_calls = 0;

    for _ in 0..2 {
        close_session_state(directory.path(), |session| {
            assert_eq!(session.kind, terminal::TerminalKind::Iterm2);
            assert_eq!(session.id, "missing-iterm-session");
            close_calls += 1;
            Ok(terminal::CloseOutcome::Missing)
        })
        .unwrap();
    }

    assert_eq!(close_calls, 1);
    assert!(!directory.path().join("terminal.json").exists());
    assert!(directory.path().join("terminal.closed.json").exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
}

#[test]
fn concurrent_close_requests_share_one_terminal_handle_claim() {
    use std::sync::{Arc, Barrier, atomic::AtomicUsize};

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "concurrent-close".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "running", None, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let first_directory = directory.path().to_owned();
    let first_barrier = Arc::clone(&barrier);
    let first_calls = Arc::clone(&calls);
    let first = std::thread::spawn(move || {
        close_session_state(&first_directory, |_| {
            first_calls.fetch_add(1, Ordering::SeqCst);
            first_barrier.wait();
            std::thread::sleep(Duration::from_millis(50));
            Ok(terminal::CloseOutcome::Closed)
        })
    });

    while !directory.path().join(TERMINAL_CLOSING_FILE).exists() {
        std::thread::yield_now();
    }
    barrier.wait();
    close_session_state(directory.path(), |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    first.join().unwrap().unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "closed"
    );
}

#[test]
fn already_closed_session_never_reuses_a_stale_terminal_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "stale-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "closed", None, None).unwrap();
    let mut close_calls = 0;

    close_session_state(directory.path(), |_| {
        close_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();

    assert_eq!(close_calls, 0);
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "closed"
    );
}

#[test]
fn explicit_close_routes_using_the_recorded_terminal_kind() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Ghostty,
            id: "ghostty-terminal".to_owned(),
            tab_id: Some("ghostty-tab".to_owned()),
            window_id: Some("ghostty-window".to_owned()),
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "working", None, None).unwrap();

    close_session_state(directory.path(), |session| {
        assert_eq!(session.kind, terminal::TerminalKind::Ghostty);
        assert_eq!(session.id, "ghostty-terminal");
        assert_eq!(session.tab_id.as_deref(), Some("ghostty-tab"));
        assert_eq!(session.window_id.as_deref(), Some("ghostty-window"));
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
}

#[test]
fn explicit_close_is_terminal_against_late_native_wrapper_updates() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join("terminal.json"),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "closing-iterm-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "running", None, None).unwrap();

    close_session_state(directory.path(), |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
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

#[cfg(target_os = "macos")]
#[test]
fn iterm_script_keeps_dynamic_values_in_argv() {
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("review this"));
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("item 1 of argv"));
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("write text bridgeCommand"));
}

#[cfg(target_os = "macos")]
#[test]
fn iterm_follow_up_sends_an_explicit_carriage_return() {
    assert!(terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("set carriageReturn to return"));
    assert!(terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("newline false"));
    assert!(!terminal::macos::iterm2::SEND_FILE_SCRIPT.contains("write text \"\""));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_cold_start_never_adopts_an_app_restored_surface() {
    assert!(
            terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains(
                "if not itermWasRunning then\n            set targetWindow to (create window with default profile)"
            )
        );
    assert!(
        terminal::macos::ghostty::CREATE_SURFACE_SCRIPT
            .contains("if not ghosttyWasRunning then\n            set targetWindow to new window")
    );
    assert!(!terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains(
        "set targetTab to do script bridgeCommand\n            set targetWindow to front window"
    ));
    for restored_surface in ["front window", "current window", "selected tab"] {
        assert!(
            !terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains(restored_surface),
            "Terminal.app cold-start path still references {restored_surface}"
        );
    }
    assert!(
        terminal::macos::apple_terminal::OPEN_TAB_SCRIPT
            .contains("set targetWindowId to my windowIdForTty(targetTty)")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_actions_require_the_recorded_window_and_tty() {
    for script in [
        terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
        terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
    ] {
        assert!(script.contains("wantedWindowId"));
        assert!(script.contains("wantedTty"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn stable_iterm_and_ghostty_ids_do_not_depend_on_mutable_display_titles() {
    for script in [
        terminal::macos::iterm2::SEND_FILE_SCRIPT,
        terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
    ] {
        assert!(script.contains("unique ID of targetSession is wantedId"));
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetSession"));
    }
    for script in [
        terminal::macos::ghostty::SEND_FILE_SCRIPT,
        terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
    ] {
        assert!(script.contains("wantedTerminalId"));
        assert!(script.contains("wantedTabId"));
        assert!(script.contains("wantedWindowId"));
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetTab"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_terminal_adapters_never_set_or_verify_display_titles() {
    for (name, script) in [
        ("iTerm2 open", terminal::macos::iterm2::OPEN_TAB_SCRIPT),
        ("iTerm2 send", terminal::macos::iterm2::SEND_FILE_SCRIPT),
        (
            "iTerm2 close",
            terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
        ),
        (
            "Ghostty create",
            terminal::macos::ghostty::CREATE_SURFACE_SCRIPT,
        ),
        (
            "Ghostty discover",
            terminal::macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
        ),
        (
            "Ghostty queue",
            terminal::macos::ghostty::QUEUE_COMMAND_SCRIPT,
        ),
        (
            "Ghostty press Enter",
            terminal::macos::ghostty::PRESS_ENTER_SCRIPT,
        ),
        ("Ghostty send", terminal::macos::ghostty::SEND_FILE_SCRIPT),
        ("Ghostty close", terminal::macos::ghostty::CLOSE_TAB_SCRIPT),
        (
            "Terminal.app open",
            terminal::macos::apple_terminal::OPEN_TAB_SCRIPT,
        ),
        (
            "Terminal.app send",
            terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        ),
        (
            "Terminal.app close",
            terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
        ),
        (
            "Terminal.app wait",
            terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
        ),
    ] {
        for forbidden in [
            "tabTitle",
            "set name",
            "set_tab_title",
            "custom title",
            "title displays custom title",
            "wantedOwnershipTitle",
        ] {
            assert!(
                !script.contains(forbidden),
                "{name} still depends on visible title fragment {forbidden:?}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_terminal_applescripts_compile_without_opening_a_tab() {
    for (name, script, application, path) in [
        (
            "iTerm2 open tab",
            terminal::macos::iterm2::OPEN_TAB_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "iTerm2 send file",
            terminal::macos::iterm2::SEND_FILE_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "iTerm2 close session",
            terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
            "iTerm2",
            "/Applications/iTerm.app",
        ),
        (
            "Terminal.app open tab",
            terminal::macos::apple_terminal::OPEN_TAB_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app send file",
            terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app close tab",
            terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Terminal.app wait for close",
            terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
            "Terminal",
            "/System/Applications/Utilities/Terminal.app",
        ),
        (
            "Ghostty create surface",
            terminal::macos::ghostty::CREATE_SURFACE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty discover terminal",
            terminal::macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty queue command",
            terminal::macos::ghostty::QUEUE_COMMAND_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty press Enter",
            terminal::macos::ghostty::PRESS_ENTER_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty send file",
            terminal::macos::ghostty::SEND_FILE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty close tab",
            terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let script = script.replace(
            &format!("tell application \"{application}\""),
            &format!("tell application \"{path}\""),
        );
        let source = directory.path().join("bridge.applescript");
        std::fs::write(&source, script).unwrap();
        let output = std::process::Command::new("/usr/bin/osacompile")
            .arg("-o")
            .arg(directory.path().join("bridge.scpt"))
            .arg(source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name} AppleScript did not compile: {}",
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
    let settings = provider::claude_hook_settings(Path::new("/opt/Agent Bridge/bin/agent-bridge"));
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["command"],
        "/opt/Agent Bridge/bin/agent-bridge"
    );
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["args"],
        serde_json::json!(["native-hook", "claude"])
    );
    assert!(settings["hooks"]["PermissionRequest"].is_null());
}

#[cfg(unix)]
#[test]
fn shell_quoting_handles_apostrophes_without_executing_them() {
    assert_eq!(
        shell_quote(std::ffi::OsStr::new("/tmp/user's bridge")),
        "'/tmp/user'\"'\"'s bridge'"
    );
}

#[cfg(windows)]
#[test]
fn windows_provider_resolution_accepts_exe_suffix() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("agy.exe"), b"stub").unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert_eq!(
        resolve_provider_from_path(FirstPartyCli::Agy, &path).unwrap(),
        directory.path().join("agy.exe").canonicalize().unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_provider_resolution_prefers_powershell_npm_shims_over_batch() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("codex"), b"#!/bin/sh\n").unwrap();
    fs::write(directory.path().join("codex.cmd"), b"@echo off\r\n").unwrap();
    fs::write(directory.path().join("codex.ps1"), b"#!/usr/bin/env pwsh\n").unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert_eq!(
        resolve_provider_from_path(FirstPartyCli::Codex, &path).unwrap(),
        directory.path().join("codex.ps1").canonicalize().unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_provider_version_check_runs_cmd_shims() {
    let directory = tempfile::tempdir().unwrap();
    let shim = directory.path().join("agy.cmd");
    fs::write(&shim, "@echo off\r\necho agy 1.1.12\r\n").unwrap();
    assert_eq!(
        check_provider_version(FirstPartyCli::Agy, &shim).unwrap(),
        "agy 1.1.12"
    );
}

#[cfg(windows)]
#[test]
fn windows_state_root_falls_back_to_userprofile_without_home() {
    assert_eq!(
        default_state_root(None, Some(std::ffi::OsStr::new(r"C:\Users\cmd-user"))).unwrap(),
        PathBuf::from(r"C:\Users\cmd-user\.agent-bridge\native-sessions")
    );
}

#[test]
fn windows_agy_brain_root_falls_back_to_userprofile_without_home() {
    assert_eq!(
        default_agy_brain_root(None, Some(std::ffi::OsStr::new(r"C:\Users\agy-user"))).unwrap(),
        PathBuf::from(r"C:\Users\agy-user\.gemini\antigravity-cli\brain")
    );
}

#[cfg(windows)]
#[test]
fn windows_batch_providers_are_launched_through_a_fixed_powershell_forwarder() {
    let directory = tempfile::tempdir().unwrap();
    let command = provider_process_command(
        Path::new(r"\\?\C:\npm\codex.cmd"),
        directory.path(),
        vec![OsString::from("--config"), OsString::from("a&b")],
    )
    .unwrap();
    assert!(Path::new(command.get_program()).is_absolute());
    assert_eq!(
        Path::new(command.get_program()).file_name().unwrap(),
        "pwsh.exe"
    );
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(args[0], "-NoLogo");
    assert_eq!(args[1], "-NoProfile");
    assert_eq!(args[2], "-ExecutionPolicy");
    assert_eq!(args[3], "Bypass");
    assert_eq!(args[4], "-File");
    assert_eq!(args[6], r"C:\npm\codex.cmd");
    assert_eq!(args[8], "a&b");
}

#[cfg(windows)]
#[test]
fn windows_batch_providers_preserve_percent_and_shell_metacharacters() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider.cmd");
    write_private(
        &provider,
        br#"@echo off
set "AGENT_BRIDGE_PROBE_1=%~1"
set "AGENT_BRIDGE_PROBE_2=%~2"
pwsh.exe -NoLogo -NoProfile -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::WriteLine('ARG=[' + $env:AGENT_BRIDGE_PROBE_1 + ']'); [Console]::WriteLine('ARG=[' + $env:AGENT_BRIDGE_PROBE_2 + ']')"
"#,
    )
    .unwrap();
    let mut command = provider_process_command(
        &provider,
        directory.path(),
        vec![
            OsString::from("prompt %SECRET_ENV% 100%"),
            OsString::from("owner's & | < > ^ !"),
        ],
    )
    .unwrap();
    command.env("SECRET_ENV", "EXPANDED");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "ARG=[prompt %SECRET_ENV% 100%]",
            "ARG=[owner's & | < > ^ !]",
        ]
    );
}

#[cfg(windows)]
#[test]
fn windows_console_helper_accepts_only_managed_session_identity() {
    assert!(matches!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "pending-prompt-1.txt",
        ])
        .unwrap(),
        NativeCommand::ConsoleControl {
            action,
            id,
            input_name: Some(input_name),
        } if action == "send" && id == "session-owner123" && input_name == "pending-prompt-1.txt"
    ));
    assert!(parse_args(["native-console-control", "close", "1234"]).is_err());
    assert!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "..\\outside.txt",
        ])
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn windows_close_control_uses_the_claimed_terminal_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(TERMINAL_HANDLE_FILE), b"active").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), "close"),
        directory.path().join(TERMINAL_HANDLE_FILE)
    );
    fs::write(directory.path().join(TERMINAL_CLOSING_FILE), b"closing").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), "close"),
        directory.path().join(TERMINAL_CLOSING_FILE)
    );
}

#[test]
fn bridge_shell_command_quotes_the_workspace_and_executable() {
    let command = bridge_shell_command(
        Path::new("/tmp/project; touch nope"),
        Path::new("/tmp/state root"),
        Path::new("/tmp/Agent Bridge/bin"),
        "session-safe123",
    )
    .unwrap();
    #[cfg(unix)]
    assert_eq!(
        command,
        "cd '/tmp/project; touch nope' && AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'"
    );
    #[cfg(windows)]
    assert_eq!(
        command,
        "Set-Location -LiteralPath '/tmp/project; touch nope'; $env:AGENT_BRIDGE_NATIVE_STATE_DIR = '/tmp/state root'; & '/tmp/Agent Bridge/bin' native-session 'session-safe123'"
    );
}

#[cfg(windows)]
#[test]
fn powershell_launch_quoting_doubles_apostrophes() {
    let command = bridge_shell_command(
        Path::new("C:\\work\\owner's repo"),
        Path::new("C:\\state"),
        Path::new("C:\\bin\\agent-bridge.exe"),
        "session-safe123",
    )
    .unwrap();
    assert!(command.contains("'C:\\work\\owner''s repo'"));
    assert!(!command.contains("owner's repo"));
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
fn every_provider_declares_its_current_follow_up_transport() {
    for provider in [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ] {
        assert_eq!(
            provider::follow_up_transport(provider),
            provider::FollowUpTransport::TerminalPasteFallback
        );
    }
}

#[test]
fn every_provider_declares_its_initial_prompt_transport() {
    for provider in [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ] {
        #[cfg(windows)]
        assert_eq!(
            provider::initial_prompt_transport(provider),
            provider::InitialPromptTransport::TerminalPasteAfterLaunch
        );
        #[cfg(not(windows))]
        assert_eq!(
            provider::initial_prompt_transport(provider),
            provider::InitialPromptTransport::ProviderArgument
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_initial_prompt_readiness_is_provider_specific() {
    assert_eq!(
        provider::initial_prompt_ready_delay(FirstPartyCli::Agy),
        Duration::from_secs(12)
    );
    for provider in [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Pi,
    ] {
        assert_eq!(
            provider::initial_prompt_ready_delay(provider),
            Duration::from_secs(2)
        );
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

#[test]
fn stale_turn_claim_cannot_release_a_new_owner() {
    let directory = tempfile::tempdir().unwrap();
    let stale = acquire_turn_claim(directory.path()).unwrap();
    release_turn_claim(directory.path()).unwrap();
    let current = acquire_turn_claim(directory.path()).unwrap();

    drop(stale);

    assert!(acquire_turn_claim(directory.path()).is_err());
    drop(current);
    assert!(acquire_turn_claim(directory.path()).is_ok());
}

#[test]
fn event_commit_does_not_create_a_racy_latest_cache() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    let event = SessionEvent {
        provider: "codex".to_owned(),
        message: "committed".to_owned(),
        error: None,
        provider_session_id: None,
        turn_id: None,
        created_unix_ms: 1,
    };

    write_event(directory.path(), &event).unwrap();

    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let stored: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(stored.message, "committed");
    assert!(!directory.path().join("latest.json").exists());
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

fn write_owned_terminal_state(directory: &Path, state: &str, owner_pid: u32) {
    fs::create_dir(directory.join("events")).unwrap();
    update_status(directory, state, None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    claim.retain();
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::AppleTerminal,
            id: "/dev/ttys999".to_owned(),
            tab_id: None,
            window_id: Some("1001".to_owned()),
            managed_session_id: Some("session-owner123".to_owned()),
            windows_process_identity: None,
        },
    )
    .unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: owner_pid,
            managed_session_id: Some("session-owner123".to_owned()),
            terminal_tty: Some("/dev/ttys999".to_owned()),
            windows_process_identity: test_windows_process_identity(owner_pid),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

fn assert_dead_terminal_owner_close_converges(state: &str) {
    let directory = tempfile::tempdir().unwrap();
    write_owned_terminal_state(directory.path(), state, reaped_child_pid());
    let mut adapter_calls = 0;

    assert!(repair_dead_native_owner(directory.path()).unwrap());
    close_session_state(directory.path(), |_| {
        adapter_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    close_session_state(directory.path(), |_| {
        adapter_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();

    assert_eq!(adapter_calls, 0);
    assert!(!directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
}

#[test]
fn exited_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("exited");
}

#[test]
fn failed_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("failed");
}

#[test]
fn exited_and_failed_live_native_owners_are_not_repaired() {
    for state in ["exited", "failed"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), state, std::process::id());

        assert!(!repair_dead_native_owner(directory.path()).unwrap());
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, state);
    }
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
            windows_process_identity: test_windows_process_identity(std::process::id()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();

    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "working");
}

#[cfg(windows)]
fn test_windows_process_identity(pid: u32) -> Option<terminal::WindowsProcessIdentity> {
    terminal::windows_process_identity(pid).ok()
}

#[cfg(not(windows))]
fn test_windows_process_identity(_pid: u32) -> Option<terminal::WindowsProcessIdentity> {
    None
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
            ..NativeSessionOwner::default()
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

#[test]
fn terminal_owner_proof_rejects_record_only_ids_and_reused_surfaces() {
    let terminal: terminal::TerminalSession = serde_json::from_value(serde_json::json!({
        "terminal": "apple-terminal",
        "session_id": "/dev/ttys001",
        "window_id": "1001",
        "managed_session_id": "session-owner123"
    }))
    .unwrap();
    let owner = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: Some("/dev/ttys001".to_owned()),
        terminal_tty_device: Some(7),
        process_start_seconds: Some(100),
        process_start_microseconds: Some(200),
        windows_process_identity: None,
    };
    let live = NativeProcessIdentity {
        pid: 4242,
        terminal_tty_device: 7,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };

    verify_terminal_owner_attestation("session-owner123", &terminal, &owner, &live, 7).unwrap();

    let record_only = NativeSessionOwner {
        pid: 4242,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: None,
        terminal_tty_device: None,
        process_start_seconds: None,
        process_start_microseconds: None,
        windows_process_identity: None,
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &record_only, &live, 7,)
            .is_err()
    );

    let wrong_session = NativeSessionOwner {
        managed_session_id: Some("session-other456".to_owned()),
        ..owner.clone()
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &wrong_session, &live, 7,)
            .is_err()
    );

    let wrong_tty = NativeSessionOwner {
        terminal_tty: Some("/dev/ttys002".to_owned()),
        ..owner.clone()
    };
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &wrong_tty, &live, 7,)
            .is_err()
    );

    let reused_process = NativeProcessIdentity {
        process_start_microseconds: 201,
        ..live
    };
    assert!(
        verify_terminal_owner_attestation(
            "session-owner123",
            &terminal,
            &owner,
            &reused_process,
            7,
        )
        .is_err()
    );
    assert!(
        verify_terminal_owner_attestation("session-owner123", &terminal, &owner, &live, 8,)
            .is_err()
    );
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
fn session_metadata_titles_drop_control_characters_and_are_bounded() {
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
fn pi_session_loads_the_result_extension_and_explicit_project_approval() {
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
            model: Some("Fable".to_owned()),
            effort: Some("minimal".to_owned()),
            yolo: true,
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();
    write_private(&directory.join("initial-prompt.txt"), b"pi prompt").unwrap();

    run_session_inner(&directory).unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(arguments.contains("--model\nanthropic/claude-fable-5"));
    assert!(arguments.contains("--thinking\nminimal"));
    assert!(arguments.contains("--extension"));
    assert!(arguments.contains("--name\nPi test"));
    assert!(arguments.ends_with("pi prompt\n"));
    assert!(arguments.contains("--approve"));
    assert!(!arguments.contains("dangerously"));
    let extension = fs::read_to_string(directory.join("pi-agent-bridge.js")).unwrap();
    assert_eq!(extension, pi_bridge_extension());
}
