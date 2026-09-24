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
fn ask_and_tell_accept_prompt_files_without_putting_prompt_text_in_argv() {
    let directory = tempfile::tempdir().unwrap();
    let prompt_path = directory.path().join("prompt.txt");
    fs::write(&prompt_path, "long prompt\r\nfrom file").unwrap();
    let prompt_path = prompt_path.to_string_lossy().into_owned();

    let ask = parse_args(["ask", "claude", "--prompt-file", &prompt_path]).unwrap();
    assert!(matches!(
        ask,
        NativeCommand::Ask(AskRequest { prompt, .. }) if prompt == "long prompt\nfrom file"
    ));
    let tell = parse_args(["tell", "session-file123", "--prompt-file", &prompt_path]).unwrap();
    assert!(matches!(
        tell,
        NativeCommand::Tell(TellRequest { prompt, .. }) if prompt == "long prompt\nfrom file"
    ));
    assert!(
        parse_args([
            "ask",
            "claude",
            "--prompt",
            "inline",
            "--prompt-file",
            &prompt_path,
        ])
        .is_err()
    );
}

#[test]
fn ask_and_tell_accept_exact_repeatable_context_result_addresses() {
    let ask = parse_args([
        "ask",
        "codex",
        "--prompt",
        "continue",
        "--context-result",
        "session-a/request-1",
        "--context-result",
        "session-b/event-2.json",
    ])
    .unwrap();
    let NativeCommand::Ask(AskRequest {
        context_results, ..
    }) = ask
    else {
        panic!("expected ask");
    };
    assert_eq!(
        context_results
            .iter()
            .map(context::ContextResultRef::address)
            .collect::<Vec<_>>(),
        ["session-a/request-1", "session-b/event-2.json"]
    );
    let tell = parse_args([
        "tell",
        "session-target",
        "--prompt",
        "continue",
        "--context-result",
        "session-a/request-1",
    ])
    .unwrap();
    assert!(matches!(
        tell,
        NativeCommand::Tell(TellRequest { context_results, .. }) if context_results.len() == 1
    ));
    let plain = parse_args(["tell", "session-target", "--prompt", "continue"]).unwrap();
    assert!(matches!(
        plain,
        NativeCommand::Tell(TellRequest { context_results, .. }) if context_results.is_empty()
    ));

    for (arguments, expected) in [
        (
            vec!["tell", "session-t", "--prompt", "x", "--context-result"],
            "requires a value",
        ),
        (
            vec![
                "tell",
                "session-t",
                "--prompt",
                "x",
                "--context-result",
                "session-a/latest",
            ],
            "invalid --context-result",
        ),
        (
            vec![
                "ask",
                "codex",
                "--prompt",
                "x",
                "--context-result",
                "session-a/request-1",
                "--context-result",
                "session-a/request-1",
            ],
            "more than once",
        ),
        (
            vec!["ask", "codex", "--context-result", "session-a/request-1"],
            "requires --prompt",
        ),
    ] {
        let error = format!("{:#}", parse_args(arguments).unwrap_err());
        assert!(error.contains(expected), "{error}");
    }
    let mut nine = vec!["ask", "codex", "--prompt", "x"];
    let addresses = (1..=9)
        .map(|index| format!("session-a/request-{index}"))
        .collect::<Vec<_>>();
    for address in &addresses {
        nine.push("--context-result");
        nine.push(address);
    }
    assert!(format!("{:#}", parse_args(nine).unwrap_err()).contains("at most 8"));
}

#[test]
fn ask_rejects_terminal_control_sequences_from_inline_and_file_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let prompt_path = directory.path().join("prompt.txt");
    fs::write(&prompt_path, "unsafe\rprompt").unwrap();
    let prompt_path = prompt_path.to_string_lossy().into_owned();

    for arguments in [
        vec!["ask", "codex", "--prompt", "unsafe\u{1b}prompt"],
        vec!["ask", "pi", "--prompt-file", prompt_path.as_str()],
    ] {
        let error = parse_args(arguments).unwrap_err();
        assert!(
            format!("{error:#}").contains("terminal control"),
            "unexpected rejection: {error:#}"
        );
    }
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
fn remaining_turn_timeout_rejects_an_exhausted_total_budget() {
    let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let error = remaining_turn_timeout(expired, Duration::from_secs(5)).unwrap_err();
    assert!(format!("{error:#}").contains("timed out after 5 seconds"));

    let future = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
    let remaining = remaining_turn_timeout(future, Duration::from_secs(5)).unwrap();
    assert!(remaining > Duration::ZERO);
    assert!(remaining <= Duration::from_secs(5));
}

#[test]
fn terminal_delivery_keeps_the_original_deadline_after_preflight_work() {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(5);

    assert_eq!(
        terminal::remaining_send_budget_at(deadline, started + Duration::from_secs(3)).unwrap(),
        Duration::from_secs(2)
    );
    assert!(terminal::remaining_send_budget_at(deadline, deadline).is_err());
}

#[cfg(unix)]
#[test]
fn bounded_command_output_terminates_a_hung_child_at_the_deadline() {
    let mut command = Command::new("/bin/sh");
    // The background child inherits stdout/stderr. A pipe-reader implementation
    // would still block after killing only the direct shell process.
    command.args(["-c", "sleep 1 & wait"]);
    let started = Instant::now();

    let failure = command_output_until_classified(
        &mut command,
        started + Duration::from_millis(100),
        "test child",
    )
    .unwrap_err();
    assert!(failure.process_started());
    let error = failure.into_error();

    assert!(format!("{error:#}").contains("test child timed out"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[cfg(unix)]
#[test]
fn bounded_command_output_identifies_a_pre_spawn_failure() {
    let mut command = Command::new("/definitely/not/an/agent-bridge-executable");

    let failure = command_output_until_classified(
        &mut command,
        Instant::now() + Duration::from_secs(1),
        "missing test child",
    )
    .unwrap_err();

    assert!(!failure.process_started());
    assert!(format!("{:#}", failure.into_error()).contains("failed to start missing test child"));
}

#[test]
fn initial_prompt_delay_must_fit_inside_the_original_ask_budget() {
    let deadline = Instant::now() + Duration::from_millis(20);
    let error = initial_prompt_delay_within_budget(
        deadline,
        Duration::from_secs(12),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("initial prompt readiness delay"));

    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        initial_prompt_delay_within_budget(
            deadline,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .unwrap(),
        Duration::from_millis(10)
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
fn monitor_failures_are_journaled_against_the_current_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();

    record_provider_monitor_failure(
        directory.path(),
        FirstPartyCli::Pi,
        "Pi result monitor stopped",
    )
    .unwrap();

    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "failed");
    assert_eq!(status.error.as_deref(), Some("Pi result monitor stopped"));
    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.error.as_deref(), Some("Pi result monitor stopped"));
}

#[test]
fn pending_completion_recovery_converges_after_every_partial_mutation() {
    for completed_mutations in 0..=3 {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();
        let event = SessionEvent {
            provider: FirstPartyCli::Codex.as_str().to_owned(),
            message: "committed result".to_owned(),
            error: None,
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            created_unix_ms: 1,
        };
        let pending = PendingTurnCompletion::new(&claim_token, event, None).unwrap();
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if completed_mutations >= 1 {
            write_pending_completion_event(directory.path(), &pending).unwrap();
        }
        if completed_mutations >= 2 {
            update_status(directory.path(), "ready", None, None).unwrap();
        }
        if completed_mutations >= 3 {
            release_turn_claim_token(&directory.path().join(TURN_CLAIM_FILE), &claim_token)
                .unwrap();
        }

        assert!(recover_pending_completion(directory.path()).unwrap());

        let paths = event_paths(directory.path()).unwrap();
        assert_eq!(paths.len(), 1);
        let stored: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(stored.message, "committed result");
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
    }
}

#[test]
fn terminal_status_cannot_regress_and_generation_is_monotonic() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), "launching", None, None).unwrap();
    update_status(directory.path(), "running", None, None).unwrap();
    let running: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    update_status(directory.path(), "exited", Some(0), None).unwrap();
    let exited: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    assert!(exited.generation > running.generation);
    assert!(update_status(directory.path(), "ready", None, None).is_err());
    let stable: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(stable.state, "exited");
    assert_eq!(stable.generation, exited.generation);
}

#[test]
fn claimed_session_can_finalize_when_the_provider_exits_before_delivery() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    update_status(directory.path(), "claimed", None, None).unwrap();

    finalize_native_session(directory.path(), &Ok(())).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "exited");
    assert_eq!(status.exit_code, Some(0));
}

#[test]
fn native_finalization_is_idempotent_after_a_terminal_status() {
    for state in ["exited", "failed", "closed"] {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        if state != "closed" {
            update_status(directory.path(), "running", None, None).unwrap();
        }
        match state {
            "exited" => update_status(directory.path(), state, Some(0), None).unwrap(),
            "failed" => update_status(
                directory.path(),
                state,
                None,
                Some("provider failed".to_owned()),
            )
            .unwrap(),
            "closed" => update_status(directory.path(), state, None, None).unwrap(),
            _ => unreachable!(),
        }
        let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

        finalize_native_session(directory.path(), &Ok(())).unwrap();

        let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(after.state, state);
        assert_eq!(after.generation, before.generation);
    }
}

#[test]
fn completion_and_process_exit_converge_in_either_serialized_order() {
    for completion_first in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        update_status(directory.path(), "running", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();

        let complete = || {
            record_provider_result_for_claim(
                directory.path(),
                FirstPartyCli::Codex,
                "completed result",
                Some("codex-session".to_owned()),
                Some("codex-turn".to_owned()),
                Some(&claim_token),
            )
            .unwrap();
        };
        if completion_first {
            complete();
            finalize_native_session(directory.path(), &Ok(())).unwrap();
        } else {
            finalize_native_session(directory.path(), &Ok(())).unwrap();
            complete();
        }

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "exited");
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert_eq!(
            event_paths(directory.path()).unwrap().len(),
            usize::from(completion_first)
        );
    }
}

#[test]
fn stale_provider_completion_cannot_release_a_replacement_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();

    record_provider_result_for_claim(
        directory.path(),
        FirstPartyCli::Claude,
        "stale result",
        Some("claude-session".to_owned()),
        None,
        Some("stale-claim-token"),
    )
    .unwrap();

    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(event_paths(directory.path()).unwrap().is_empty());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "working");
}

#[test]
fn duplicate_provider_turn_cannot_release_a_replacement_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let first_claim = acquire_turn_claim(directory.path()).unwrap();
    first_claim.retain();
    record_provider_result(
        directory.path(),
        FirstPartyCli::Codex,
        "first result",
        Some("codex-session".to_owned()),
        Some("codex-turn".to_owned()),
    )
    .unwrap();
    let replacement_claim = acquire_turn_claim(directory.path()).unwrap();
    replacement_claim.retain();
    update_status(directory.path(), "claimed", None, None).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();

    record_provider_result(
        directory.path(),
        FirstPartyCli::Codex,
        "duplicate result",
        Some("codex-session".to_owned()),
        Some("codex-turn".to_owned()),
    )
    .unwrap();

    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "working");
}

#[test]
fn correlated_wait_ignores_other_completed_turns() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token.clone();
    claim.retain();
    for (turn_id, message) in [
        ("claude-turn-other", "other result"),
        ("claude-turn-expected", "expected result"),
    ] {
        write_event(
            directory.path(),
            &SessionEvent {
                provider: "claude".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: Some("claude-session".to_owned()),
                turn_id: Some(turn_id.to_owned()),
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
    }
    update_status(directory.path(), "ready", None, None).unwrap();
    release_turn_claim(directory.path()).unwrap();

    let event = wait_for_event_for_turn(
        directory.path(),
        0,
        Some("claude-turn-expected"),
        Some(&claim_token),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(event.message, "expected result");
}

#[test]
fn completed_event_is_not_published_until_the_session_is_ready() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: std::process::id(),
            windows_process_identity: test_windows_process_identity(std::process::id()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    write_event(
        directory.path(),
        &SessionEvent {
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: Some("codex-session".to_owned()),
            turn_id: Some("codex-turn".to_owned()),
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();

    let error = wait_for_event(directory.path(), 0, Duration::ZERO).unwrap_err();

    assert!(format!("{error:#}").contains("timed out"));
}

#[test]
fn completed_event_uses_its_own_released_claim_not_a_later_turns_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let completed_claim = acquire_turn_claim(directory.path()).unwrap();
    let completed_token = completed_claim.token.clone();
    write_event(
        directory.path(),
        &SessionEvent {
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    release_turn_claim(directory.path()).unwrap();
    let later_claim = acquire_turn_claim(directory.path()).unwrap();
    later_claim.retain();
    update_status(directory.path(), "claimed", None, None).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();

    let event = wait_for_event_for_turn(
        directory.path(),
        0,
        None,
        Some(&completed_token),
        Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(event.message, "completed result");
}

#[test]
fn completed_event_waits_for_its_own_claim_to_be_released() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token.clone();
    claim.retain();
    write_event(
        directory.path(),
        &SessionEvent {
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();

    let error = wait_for_event_for_turn(
        directory.path(),
        0,
        None,
        Some(&claim_token),
        Duration::ZERO,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("timed out"));
}

#[test]
fn uncorrelated_wait_returns_the_first_event_after_its_baseline() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    for message in ["expected turn", "later turn"] {
        write_event(
            directory.path(),
            &SessionEvent {
                provider: "codex".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: unix_ms(),
            },
        )
        .unwrap();
    }

    let event = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap();

    assert_eq!(event.message, "expected turn");
}

fn write_resume_wait_owner(directory: &Path, state: &str, pid: u32) {
    update_status(directory, state, None, None).unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            windows_process_identity: test_windows_process_identity(pid),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

#[test]
fn close_during_turn_claim_cannot_resurrect_the_session() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let waiter_directory = directory.path().to_owned();
    let waiter = thread::spawn(move || {
        acquire_ready_turn_claim_after_claim(&waiter_directory, "session-closing123", || {
            claimed_tx.send(()).unwrap();
            continue_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            Ok(())
        })
    });
    claimed_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    mark_session_closed(directory.path(), None).unwrap();
    continue_tx.send(()).unwrap();
    let error = match waiter.join().unwrap() {
        Ok(_) => panic!("closed session unexpectedly reacquired a turn"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("claim disappeared"));
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn close_cannot_interleave_between_ready_validation_and_claimed_publication() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    let (publish_tx, publish_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let tell_directory = directory.path().to_owned();
    let tell = thread::spawn(move || {
        acquire_ready_turn_claim_with_callbacks(
            &tell_directory,
            "session-closing123",
            || Ok(()),
            || {
                publish_tx.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            },
            &[],
        )
    });
    publish_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let close_directory = directory.path().to_owned();
    let close = thread::spawn(move || mark_session_closed(&close_directory, None));

    continue_tx.send(()).unwrap();
    let (claim, _) = tell.join().unwrap().unwrap();
    drop(claim);
    close.join().unwrap().unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn failed_closed_publication_keeps_legacy_resume_and_claim_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    fs::write(directory.path().join(LEGACY_RESUME_PENDING_FILE), "pending").unwrap();
    fs::write(directory.path().join(LEGACY_RESUME_RUNNING_FILE), "running").unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    fs::remove_file(directory.path().join("status.json")).unwrap();
    fs::create_dir(directory.path().join("status.json")).unwrap();

    assert!(mark_session_closed(directory.path(), None).is_err());

    assert!(directory.path().join(LEGACY_RESUME_PENDING_FILE).exists());
    assert!(directory.path().join(LEGACY_RESUME_RUNNING_FILE).exists());
    assert_eq!(
        fs::read_to_string(directory.path().join(TURN_CLAIM_FILE))
            .unwrap()
            .trim(),
        claim.token
    );
}

#[test]
fn closed_cleanup_attempts_every_legacy_capability_release() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_resume_wait_owner(directory.path(), "ready", std::process::id());
    fs::create_dir(directory.path().join(LEGACY_RESUME_PENDING_FILE)).unwrap();
    fs::write(directory.path().join(LEGACY_RESUME_RUNNING_FILE), "running").unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();

    assert!(mark_session_closed(directory.path(), None).is_err());

    assert!(!directory.path().join(LEGACY_RESUME_RUNNING_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn close_is_rejected_without_the_explicit_flag() {
    assert!(parse_args(["close-session", "session-safe123"]).is_err());
    assert!(parse_args(["close-session", "session-safe123", "--explicit"]).is_ok());
}

#[test]
fn prune_sessions_requires_an_explicit_positive_retention_window() {
    assert!(parse_args(["prune-sessions", "--closed-before-days", "30"]).is_err());
    assert!(parse_args(["prune-sessions", "--closed-before-days", "0", "--explicit",]).is_err());
    assert!(parse_args(["prune-sessions", "--closed-before-days", "30", "--explicit",]).is_ok());
}

#[test]
fn search_rejects_empty_queries_conflicting_scopes_and_out_of_range_limits() {
    for args in [
        vec!["search"],
        vec!["search", ""],
        vec!["search", " 	 "],
        vec!["search", "needle", "--workspace", ".", "--all-workspaces"],
        vec!["search", "needle", "--all-workspaces", "--all-workspaces"],
        vec!["search", "needle", "--limit", "0"],
        vec!["search", "needle", "--limit", "201"],
        vec!["search", "needle", "--limit", "many"],
        vec!["search", "needle", "--limit"],
        vec!["search", "needle", "--provider", "gpt"],
        vec!["search", "needle", "--unknown"],
    ] {
        assert!(parse_args(args.clone()).is_err(), "{args:?}");
    }
    let error = parse_args(["search", "needle", "--workspace", ".", "--all-workspaces"])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only one of --workspace or --all-workspaces"),
        "{error}"
    );
    let error = parse_args(["search", "needle", "--limit", "201"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("between 1 and 200"), "{error}");
    assert!(matches!(
        parse_args([
            "search",
            "needle",
            "--all-workspaces",
            "--limit",
            "200",
            "--json"
        ])
        .unwrap(),
        NativeCommand::Search(_)
    ));
    assert!(parse_args(["search", "needle", "--workspace", "."]).is_ok());
}

fn write_prune_test_session(
    root: &Path,
    id: &str,
    state: &str,
    closed_unix_ms: Option<u128>,
) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: id.to_owned(),
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("/opt/codex"),
            provider_version: "codex-cli 0.147.0".to_owned(),
            workspace: root.to_owned(),
            title: id.to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
    let status = SessionStatus {
        state: state.to_owned(),
        generation: 1,
        updated_unix_ms: closed_unix_ms.unwrap_or(1_000),
        exit_code: None,
        error: None,
    };
    write_json_atomic(&directory.join("status.json"), &status).unwrap();
    if let Some(updated_unix_ms) = closed_unix_ms {
        write_json_atomic(
            &directory.join(CLOSED_STATUS_FILE),
            &SessionStatus {
                state: "closed".to_owned(),
                generation: 1,
                updated_unix_ms,
                exit_code: None,
                error: None,
            },
        )
        .unwrap();
    }
    directory
}

#[test]
fn pruning_removes_only_old_quiescent_closed_session_directories() {
    let root = tempfile::tempdir().unwrap();
    let old = write_prune_test_session(root.path(), "session-old123", "closed", Some(100));
    let recent = write_prune_test_session(root.path(), "session-recent123", "closed", Some(900));
    let live = write_prune_test_session(root.path(), "session-live123", "ready", None);
    let claimed = write_prune_test_session(root.path(), "session-claimed123", "closed", Some(100));
    fs::write(claimed.join(TURN_CLAIM_FILE), "still-owned").unwrap();
    #[cfg(not(windows))]
    let owned = {
        let owned = write_prune_test_session(root.path(), "session-owned123", "closed", Some(100));
        write_json_atomic(
            &owned.join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: std::process::id(),
                managed_session_id: Some("session-owned123".to_owned()),
                ..NativeSessionOwner::default()
            },
        )
        .unwrap();
        owned
    };
    #[cfg(windows)]
    let owned = {
        let owned = write_prune_test_session(root.path(), "session-owned123", "closed", Some(100));
        write_json_atomic(
            &owned.join(SESSION_OWNER_FILE),
            &NativeSessionOwner {
                pid: std::process::id(),
                managed_session_id: Some("session-owned123".to_owned()),
                windows_process_identity: Some(
                    terminal::windows_process_identity(std::process::id()).unwrap(),
                ),
                ..NativeSessionOwner::default()
            },
        )
        .unwrap();
        owned
    };

    let removed = prune_closed_sessions(root.path(), 500).unwrap();

    assert_eq!(removed, ["session-old123"]);
    assert!(!old.exists());
    assert!(recent.exists());
    assert!(live.exists());
    assert!(claimed.exists());
    assert!(owned.exists());
}

#[test]
fn pruning_skips_malformed_closed_records_without_blocking_valid_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let valid = write_prune_test_session(root.path(), "session-valid123", "closed", Some(100));
    let malformed = write_prune_test_session(root.path(), "session-broken123", "closed", Some(100));
    fs::write(malformed.join(CLOSED_STATUS_FILE), "not-json").unwrap();

    let removed = prune_closed_sessions(root.path(), 500).unwrap();

    assert_eq!(removed, ["session-valid123"]);
    assert!(!valid.exists());
    assert!(malformed.exists());
}

#[cfg(unix)]
#[test]
fn pruning_never_follows_a_session_directory_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = write_prune_test_session(outside.path(), "session-target123", "closed", Some(100));
    symlink(&target, root.path().join("session-link123")).unwrap();

    assert!(prune_closed_sessions(root.path(), 500).unwrap().is_empty());
    assert!(target.exists());
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
fn explicit_close_remains_available_with_a_corrupt_completion_journal() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();
    fs::write(directory.path().join(TURN_COMPLETION_FILE), "not-json").unwrap();

    close_repaired_session_state(directory.path(), |_| {
        panic!("a missing terminal handle must not call the adapter")
    })
    .unwrap();

    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid pending native turn completion"))
    );
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
fn explicit_close_restores_the_terminal_handle_after_adapter_failure() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::WindowsConsole,
            id: "windows-console-process".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: Some("session-windows-close".to_owned()),
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();

    let error = close_session_state(directory.path(), |_| {
        Err(anyhow::anyhow!(
            "failed to attach to the managed console process: Access is denied"
        ))
    })
    .unwrap_err();

    assert!(error.to_string().contains("Access is denied"));
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "ready"
    );
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
fn interrupted_terminal_close_resumes_from_the_claimed_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_CLOSING_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "interrupted-close".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let mut calls = 0;

    close_session_state(directory.path(), |session| {
        calls += 1;
        assert_eq!(session.id, "interrupted-close");
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();

    assert_eq!(calls, 1);
    assert!(!directory.path().join(TERMINAL_CLOSING_FILE).exists());
    assert!(directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "closed"
    );
}

#[test]
fn tell_cannot_claim_while_explicit_close_owns_the_terminal_lifecycle() {
    use std::sync::{Arc, Barrier};

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    write_json_atomic(
        &directory.path().join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: terminal::TerminalKind::Iterm2,
            id: "closing-before-tell".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let close_directory = directory.path().to_owned();
    let close_barrier = Arc::clone(&barrier);
    let close = thread::spawn(move || {
        close_session_state(&close_directory, |_| {
            close_barrier.wait();
            Ok(terminal::CloseOutcome::Closed)
        })
    });

    barrier.wait();
    let tell_directory = directory.path().to_owned();
    let tell = thread::spawn(move || acquire_ready_turn_claim(&tell_directory, "session-close123"));
    close.join().unwrap().unwrap();
    let tell_error = match tell.join().unwrap() {
        Ok(_) => panic!("tell claimed a session while close owned its lifecycle"),
        Err(error) => error,
    };

    assert!(tell_error.to_string().contains("closed"));
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
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

#[cfg(target_os = "macos")]
#[test]
fn iterm_script_keeps_dynamic_values_in_argv() {
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("review this"));
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("bridgeCommand"));
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("write text"));
    assert!(terminal::macos::iterm2::START_SESSION_SCRIPT.contains("item 2 of argv"));
    assert!(terminal::macos::iterm2::START_SESSION_SCRIPT.contains("write text bridgeCommand"));
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
fn terminal_app_window_discovery_snapshots_and_skips_stale_window_references() {
    let script = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(script.contains("set candidateWindows to get windows"));
    assert!(script.contains("repeat with candidateWindow in candidateWindows"));
    assert!(script.contains("set candidateTabs to get tabs of candidateWindow"));
    assert!(script.contains("repeat with candidateTab in candidateTabs"));
    assert!(script.contains("try\n                set candidateTabs"));
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_actions_require_the_recorded_window_and_tty() {
    for script in [
        terminal::macos::apple_terminal::START_SESSION_SCRIPT,
        terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
        terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT,
    ] {
        assert!(script.contains("wantedWindowId"));
        assert!(script.contains("wantedTty"));
    }
    assert!(terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains("do script \"\""));
    assert!(!terminal::macos::apple_terminal::OPEN_TAB_SCRIPT.contains("bridgeCommand"));
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_close_waits_for_attested_process_group_shutdown_without_key_injection() {
    let script = terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT;
    assert!(script.contains("repeat 60 times"));
    assert!(!script.contains("character id 3"));
    assert!(!script.contains("do script controlC"));

    assert_eq!(
        terminal::macos::apple_terminal::process_group_signal_target(4242).unwrap(),
        -4242
    );
    assert!(terminal::macos::apple_terminal::process_group_signal_target(0).is_err());
    assert!(
        terminal::macos::apple_terminal::process_group_signal_target(i32::MAX as u32 + 1).is_err()
    );
    assert_eq!(
        terminal::macos::apple_terminal::close_signal_plan(4242, 4000).unwrap(),
        [(-4242, libc::SIGTERM), (-4000, libc::SIGKILL)]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn stable_iterm_and_ghostty_ids_do_not_depend_on_mutable_display_titles() {
    for script in [
        terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
        terminal::macos::iterm2::SEND_FILE_SCRIPT,
        terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
    ] {
        assert!(script.contains("unique ID of targetSession is wantedId"));
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetSession"));
    }
    for script in [
        terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
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
        (
            "iTerm2 verify",
            terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
        ),
        ("iTerm2 send", terminal::macos::iterm2::SEND_FILE_SCRIPT),
        (
            "iTerm2 close",
            terminal::macos::iterm2::CLOSE_SESSION_SCRIPT,
        ),
        ("Ghostty version", terminal::macos::ghostty::VERSION_SCRIPT),
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
        (
            "Ghostty verify",
            terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
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
            "Terminal.app verify",
            terminal::macos::apple_terminal::VERIFY_TAB_SCRIPT,
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
            "iTerm2 start session",
            terminal::macos::iterm2::START_SESSION_SCRIPT,
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
            "iTerm2 verify session",
            terminal::macos::iterm2::VERIFY_SESSION_SCRIPT,
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
            "Terminal.app start session",
            terminal::macos::apple_terminal::START_SESSION_SCRIPT,
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
            "Terminal.app verify tab",
            terminal::macos::apple_terminal::VERIFY_TAB_SCRIPT,
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
            "Ghostty version",
            terminal::macos::ghostty::VERSION_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
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
            "Ghostty verify surface",
            terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty close tab",
            terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
            "Ghostty",
            "/Applications/Ghostty.app",
        ),
        (
            "Ghostty close created tab",
            terminal::macos::ghostty::CLOSE_CREATED_TAB_SCRIPT,
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
    update_status(directory.path(), "running", None, None).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
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
    let arguments = vec![
        OsString::from("prompt %SECRET_ENV% 100%"),
        OsString::from("owner's & | < > ^ !"),
    ];
    provider_process_command(&provider, directory.path(), arguments.clone()).unwrap();
    let mut command = provider_process_command(&provider, directory.path(), arguments).unwrap();
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
            "5000",
        ])
        .unwrap(),
        NativeCommand::ConsoleControl {
            action,
            id,
            input_name: Some(input_name),
            timeout_ms: Some(5000),
        } if action == "send" && id == "session-owner123" && input_name == "pending-prompt-1.txt"
    ));
    assert!(parse_args(["native-console-control", "close", "1234"]).is_err());
    assert!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "..\\outside.txt",
            "5000",
        ])
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn windows_terminal_input_requires_the_live_native_session_owner() {
    let directory = tempfile::tempdir().unwrap();
    let owner = current_native_session_owner("session-owner123").unwrap();
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();

    verified_windows_native_owner(directory.path(), "session-owner123").unwrap();
    assert!(
        verified_windows_native_owner(directory.path(), "session-other123")
            .unwrap_err()
            .to_string()
            .contains("not bound")
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
        "cd '/tmp/project; touch nope' && AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'; bridge_status=$?; exit \"$bridge_status\""
    );
    #[cfg(windows)]
    assert_eq!(
        command,
        "Set-Location -LiteralPath '/tmp/project; touch nope' -ErrorAction Stop; $env:AGENT_BRIDGE_NATIVE_STATE_DIR = '/tmp/state root'; & '/tmp/Agent Bridge/bin' native-session 'session-safe123'; exit $LASTEXITCODE"
    );
}

#[test]
fn managed_shell_exits_after_the_native_session_instead_of_accepting_late_input() {
    let command = bridge_shell_command(
        Path::new("/tmp/workspace"),
        Path::new("/tmp/state"),
        Path::new("/tmp/bridge"),
        "session-safe123",
    )
    .unwrap();

    #[cfg(unix)]
    assert!(command.ends_with("bridge_status=$?; exit \"$bridge_status\""));
    #[cfg(windows)]
    assert!(command.ends_with("exit $LASTEXITCODE"));
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
    assert_eq!(
        provider::follow_up_transport(FirstPartyCli::Codex).as_str(),
        "provider-cross-session-message-with-terminal-paste-fallback"
    );
    for provider in [FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(
            provider::follow_up_transport(provider),
            provider::FollowUpTransport::TerminalPasteFallback
        );
    }
    assert_eq!(
        provider::follow_up_transport(FirstPartyCli::Claude).as_str(),
        "provider-cross-session-message"
    );
}

#[test]
fn provider_cross_session_failures_keep_terminal_fallback_explicit() {
    let unavailable = provider::CrossSessionMessageFailure::terminal_fallback(anyhow::anyhow!(
        "native transport unavailable"
    ));
    assert!(unavailable.allows_terminal_fallback());
    assert!(!unavailable.delivery_may_have_occurred());

    let uncertain = provider::CrossSessionMessageFailure::delivery_uncertain(anyhow::anyhow!(
        "delivery uncertain"
    ));
    assert!(!uncertain.allows_terminal_fallback());
    assert!(uncertain.delivery_may_have_occurred());
}

#[test]
fn codex_hybrid_transport_falls_back_before_delivery_but_not_after_uncertainty() {
    let transport =
        provider::FollowUpTransport::ProviderCrossSessionMessageWithTerminalPasteFallback;
    let unavailable = provider::CrossSessionMessageFailure::terminal_fallback(anyhow::anyhow!(
        "daemon unavailable"
    ));
    assert_eq!(
        cross_session_failure_action(transport, &unavailable),
        CrossSessionFailureAction::TerminalFallback
    );

    let uncertain = provider::CrossSessionMessageFailure::delivery_uncertain(anyhow::anyhow!(
        "queue timed out"
    ));
    assert_eq!(
        cross_session_failure_action(transport, &uncertain),
        CrossSessionFailureAction::RetainClaim
    );

    let claude_not_sent =
        provider::CrossSessionMessageFailure::not_sent(anyhow::anyhow!("Claude discovery failed"));
    assert_eq!(
        cross_session_failure_action(
            provider::FollowUpTransport::ProviderCrossSessionMessage,
            &claude_not_sent,
        ),
        CrossSessionFailureAction::ReturnError
    );

    let claude_misclassified_fallback = provider::CrossSessionMessageFailure::terminal_fallback(
        anyhow::anyhow!("must stay provider-native"),
    );
    assert_eq!(
        cross_session_failure_action(
            provider::FollowUpTransport::ProviderCrossSessionMessage,
            &claude_misclassified_fallback,
        ),
        CrossSessionFailureAction::ReturnError
    );
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
            if provider == FirstPartyCli::Claude {
                provider::InitialPromptTransport::ProviderCrossSessionMessageAfterLaunch
            } else {
                provider::InitialPromptTransport::TerminalPasteAfterLaunch
            }
        );
        #[cfg(not(windows))]
        assert_eq!(
            provider::initial_prompt_transport(provider),
            provider::InitialPromptTransport::ProviderArgument
        );
    }
}

#[test]
fn every_provider_declares_its_terminal_submission_count() {
    assert_eq!(provider::terminal_submit_count(FirstPartyCli::Codex), 2);
    for provider in [FirstPartyCli::Claude, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert_eq!(provider::terminal_submit_count(provider), 1);
    }
}

#[test]
fn windows_initial_prompt_readiness_is_provider_specific() {
    assert_eq!(
        provider::initial_prompt_ready_delay(FirstPartyCli::Codex),
        Duration::from_secs(12)
    );
    // Agy gates its Windows console paste on its own startup log inside the adapter
    // (issue #43); the shared launcher no longer sleeps for it.
    assert_eq!(
        provider::initial_prompt_ready_delay(FirstPartyCli::Agy),
        Duration::ZERO
    );
    for provider in [FirstPartyCli::Claude, FirstPartyCli::Pi] {
        assert_eq!(
            provider::initial_prompt_ready_delay(provider),
            Duration::from_secs(2)
        );
    }
}

#[test]
fn terminal_input_framing_matches_each_adapter_contract() {
    assert_eq!(
        terminal_input_bytes(terminal::TerminalKind::Ghostty, "line one\nline two"),
        b"line one\nline two"
    );
    assert_eq!(
        terminal_input_bytes(terminal::TerminalKind::WindowsConsole, "line one\nline two"),
        b"line one\nline two"
    );
    for kind in [
        terminal::TerminalKind::Iterm2,
        terminal::TerminalKind::AppleTerminal,
    ] {
        assert_eq!(
            terminal_input_bytes(kind, "line one\nline two"),
            b"\x1b[200~line one\nline two\x1b[201~"
        );
    }
}

#[test]
fn windows_console_access_denied_is_not_a_missing_surface() {
    assert!(terminal::windows_console_helper_reports_missing(
        "Error: console process is no longer available"
    ));
    assert!(!terminal::windows_console_helper_reports_missing(
        "Error: console process is no longer available: Access is denied. (os error 5)"
    ));
}

#[test]
fn windows_console_extra_submit_waits_for_paste_confirmation() {
    assert_eq!(
        terminal::windows_console_extra_submit_delay(),
        Duration::from_secs(2)
    );
}

#[test]
fn windows_console_separates_codex_paste_confirmation_from_the_text_batch() {
    assert_eq!(terminal::windows_console_immediate_submit_count(1), 1);
    assert_eq!(terminal::windows_console_immediate_submit_count(2), 0);
}

#[test]
fn windows_console_rejects_a_submit_plan_that_cannot_fit_the_remaining_budget() {
    assert!(!terminal::windows_console_submit_delays_fit(
        2,
        Duration::from_secs(4)
    ));
    assert!(terminal::windows_console_submit_delays_fit(
        2,
        Duration::from_secs(5)
    ));
    assert!(terminal::windows_console_submit_delays_fit(
        1,
        Duration::from_millis(1)
    ));
}

#[test]
fn terminal_delivery_preflight_rejects_only_the_unstarted_windows_submit_plan() {
    assert!(
        provider::validate_terminal_send_budget_for_platform(
            FirstPartyCli::Codex,
            terminal::TerminalKind::WindowsConsole,
            Duration::from_secs(4),
            true,
        )
        .is_err()
    );
    assert!(
        provider::validate_terminal_send_budget_for_platform(
            FirstPartyCli::Codex,
            terminal::TerminalKind::WindowsConsole,
            Duration::from_secs(5),
            true,
        )
        .is_ok()
    );
    assert!(
        provider::validate_terminal_send_budget_for_platform(
            FirstPartyCli::Agy,
            terminal::TerminalKind::WindowsConsole,
            Duration::from_millis(1),
            true,
        )
        .is_ok()
    );
    assert!(
        provider::validate_terminal_send_budget_for_platform(
            FirstPartyCli::Codex,
            terminal::TerminalKind::Iterm2,
            Duration::from_millis(1),
            false,
        )
        .is_ok()
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
fn claim_release_waits_for_the_lifecycle_lock_before_deleting() {
    let directory = tempfile::tempdir().unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let path = directory.path().join(TURN_CLAIM_FILE);
    let lifecycle_lock = lock_turn_claim(&path).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();

    let release = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        drop(claim);
        finished_tx.send(()).unwrap();
    });
    started_rx.recv().unwrap();
    assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());

    drop(lifecycle_lock);
    finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    release.join().unwrap();
    assert!(!path.exists());
}

#[test]
fn unretained_tell_claim_restores_ready_state() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();

    let (claim, baseline) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    assert_eq!(baseline, 0);
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "claimed");

    drop(claim);
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn initial_prompt_failures_distinguish_safe_abort_from_uncertain_delivery() {
    for delivery_started in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), "awaiting-initial-input", None, None).unwrap();
        let mut claim = acquire_turn_claim(directory.path()).unwrap();
        if delivery_started {
            update_status(directory.path(), "working", None, None).unwrap();
        }

        record_initial_prompt_delivery_failure(
            directory.path(),
            &mut claim,
            delivery_started,
            &anyhow::anyhow!("terminal delivery failed"),
        );
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_started {
            assert_eq!(status.state, "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        } else {
            assert_eq!(status.state, "failed");
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        }
        assert_eq!(status.error.as_deref(), Some("terminal delivery failed"));
    }
}

#[test]
fn follow_up_terminal_send_failure_releases_only_confirmed_not_started_claims() {
    for delivery_may_have_occurred in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "ready", None, None).unwrap();
        let (mut claim, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let failure = if delivery_may_have_occurred {
            terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
                "terminal delivery uncertain"
            ))
        } else {
            terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "terminal delivery did not start"
            ))
        };

        record_follow_up_terminal_delivery_failure(directory.path(), &mut claim, &failure);
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_may_have_occurred {
            assert_eq!(status.state, "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(status.error.as_deref(), Some("terminal delivery uncertain"));
        } else {
            assert_eq!(status.state, "ready");
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(status.error, None);
        }
    }
}

#[test]
fn follow_up_cross_session_uncertainty_keeps_the_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    let (mut claim, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    update_status(directory.path(), "working", None, None).unwrap();

    record_cross_session_delivery_uncertainty(
        directory.path(),
        &mut claim,
        &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
    );
    drop(claim);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "working");
    assert_eq!(
        status.error.as_deref(),
        Some("delivery unconfirmed: executed input was not reported")
    );
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn late_cross_session_uncertainty_cannot_write_into_a_newer_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    let (mut delivered, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    // The target completed the delivered turn while its sender was still settling.
    release_turn_claim(directory.path()).unwrap();
    update_status(directory.path(), "ready", None, None).unwrap();
    let (newer, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();

    record_cross_session_delivery_uncertainty(
        directory.path(),
        &mut delivered,
        &anyhow::anyhow!("late report for the completed turn"),
    );
    drop(delivered);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "claimed");
    assert_eq!(status.error, None);
    assert_eq!(
        current_turn_claim_token(directory.path()).unwrap(),
        Some(newer.token.clone())
    );
    newer.retain();
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
    let windows_process_identity = test_windows_process_identity(owner_pid).or_else(|| {
        cfg!(windows).then(|| terminal::WindowsProcessIdentity {
            creation_time: 0,
            executable_path: String::new(),
        })
    });
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &terminal::TerminalSession {
            kind: if cfg!(windows) {
                terminal::TerminalKind::WindowsConsole
            } else {
                terminal::TerminalKind::AppleTerminal
            },
            id: if cfg!(windows) {
                owner_pid.to_string()
            } else {
                "/dev/ttys999".to_owned()
            },
            tab_id: None,
            window_id: (!cfg!(windows)).then(|| "1001".to_owned()),
            managed_session_id: Some("session-owner123".to_owned()),
            windows_process_identity: windows_process_identity.clone(),
        },
    )
    .unwrap();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: owner_pid,
            managed_session_id: Some("session-owner123".to_owned()),
            terminal_tty: (!cfg!(windows)).then(|| "/dev/ttys999".to_owned()),
            windows_process_identity,
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
fn terminal_close_transaction_preserves_its_terminal_reason() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();

    close_session_state_with_error(
        directory.path(),
        Some("native session process stopped".to_owned()),
        |_| panic!("a missing terminal handle must not call the adapter"),
    )
    .unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert_eq!(
        status.error.as_deref(),
        Some("native session process stopped")
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn resume_pending_dead_owner_is_repaired_instead_of_stalling() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "resume-pending", None, None).unwrap();
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
}

#[test]
fn claimed_and_awaiting_initial_input_dead_owners_are_repaired() {
    for state in ["claimed", "awaiting-initial-input"] {
        let directory = tempfile::tempdir().unwrap();
        write_owned_terminal_state(directory.path(), state, reaped_child_pid());

        #[cfg(windows)]
        assert!(
            repair_dead_native_owner_with_terminal_close(directory.path(), |session| {
                assert_eq!(session.kind, terminal::TerminalKind::WindowsConsole);
                Ok(terminal::CloseOutcome::Missing)
            })
            .unwrap()
        );
        #[cfg(not(windows))]
        assert!(repair_dead_native_owner(directory.path()).unwrap());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn explicit_close_allows_only_bound_ownerless_launch_failures() {
    let session = terminal::TerminalSession {
        kind: if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::Iterm2
        },
        id: "launch-surface".to_owned(),
        tab_id: None,
        window_id: None,
        managed_session_id: Some("session-owner123".to_owned()),
        windows_process_identity: None,
    };

    let directory = tempfile::tempdir().unwrap();
    update_status(
        directory.path(),
        "failed",
        None,
        Some("startup failed".to_owned()),
    )
    .unwrap();
    assert!(
        !verify_terminal_close_authority(directory.path(), "session-owner123", &session).unwrap()
    );

    let ready_directory = tempfile::tempdir().unwrap();
    update_status(ready_directory.path(), "ready", None, None).unwrap();
    assert!(
        verify_terminal_close_authority(ready_directory.path(), "session-owner123", &session)
            .is_err()
    );

    let launching_directory = tempfile::tempdir().unwrap();
    update_status(launching_directory.path(), "launching", None, None).unwrap();
    assert!(
        verify_terminal_close_authority(
            launching_directory.path(),
            "session-different456",
            &session,
        )
        .is_err()
    );
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
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        terminal_shell: None,
        windows_process_identity: None,
    };
    let live = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
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
        process_group: None,
        terminal_process_group: None,
        terminal_shell: None,
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
    assert!(native_owner_identity_matches(&owner, &live));
    assert!(!native_owner_identity_matches(&owner, &reused_process));
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

#[test]
fn terminal_owner_process_group_must_match_the_attested_foreground_group() {
    let owner = NativeSessionOwner {
        pid: 4242,
        process_group: Some(4242),
        terminal_process_group: Some(4242),
        ..NativeSessionOwner::default()
    };
    let live = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };

    assert_eq!(
        verified_terminal_owner_process_group(&owner, &live).unwrap(),
        4242
    );

    let changed_foreground_group = NativeProcessIdentity {
        terminal_process_group: 7777,
        ..live
    };
    assert!(verified_terminal_owner_process_group(&owner, &changed_foreground_group).is_err());

    let missing_group = NativeSessionOwner {
        process_group: None,
        terminal_process_group: None,
        ..owner
    };
    assert_eq!(
        verified_terminal_owner_process_group(&missing_group, &live).unwrap(),
        4242
    );

    let partial_group = NativeSessionOwner {
        process_group: Some(4242),
        terminal_process_group: None,
        ..missing_group
    };
    assert!(verified_terminal_owner_process_group(&partial_group, &live).is_err());
}

#[test]
fn terminal_shell_process_group_must_match_the_live_owner_parent_and_tty() {
    let shell = MacTerminalShellIdentity {
        pid: 4000,
        process_group: 4000,
        terminal_tty_device: 7,
        process_start_seconds: 90,
        process_start_microseconds: 100,
    };
    let owner = NativeSessionOwner {
        pid: 4242,
        terminal_shell: Some(shell.clone()),
        ..NativeSessionOwner::default()
    };
    let live_owner = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 100,
        process_start_microseconds: 200,
    };
    let live_shell = NativeProcessIdentity {
        pid: 4000,
        parent_pid: 3999,
        terminal_tty_device: 7,
        process_group: 4000,
        terminal_process_group: 4242,
        process_start_seconds: 90,
        process_start_microseconds: 100,
    };

    assert_eq!(
        verified_terminal_shell_process_group(&owner, &live_owner, &live_shell).unwrap(),
        4000
    );

    let wrong_tty = NativeProcessIdentity {
        terminal_tty_device: 8,
        ..live_shell
    };
    assert!(verified_terminal_shell_process_group(&owner, &live_owner, &wrong_tty).is_err());

    let changed_shell = NativeProcessIdentity {
        process_start_microseconds: 101,
        ..live_shell
    };
    assert!(verified_terminal_shell_process_group(&owner, &live_owner, &changed_shell).is_err());
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
            model: Some("openai-codex/gpt-5.6-sol".to_owned()),
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

    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(arguments.contains("--dangerously-bypass-approvals-and-sandbox"));
    assert!(arguments.contains("--model\ngpt-5.6-sol"));
    assert!(!arguments.contains("openai-codex/gpt-5.6-sol"));
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
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '2.1.234 (Claude Code)\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\n",
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
            provider_version: "2.1.234 (Claude Code)".to_owned(),
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

    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(arguments.contains("--model\nFable"));
    assert!(!arguments.contains("Fable5"));
    assert!(arguments.contains("--effort\nhigh"));
    assert!(arguments.contains("--settings"));
    assert!(arguments.ends_with("claude prompt\n"));
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

    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

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

    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(arguments.contains("--model\nanthropic/claude-fable-5"));
    assert!(arguments.contains("--thinking\nminimal"));
    assert!(arguments.contains("--extension"));
    assert!(arguments.contains("--name\nPi test"));
    assert!(arguments.contains("pi prompt"));
    assert!(arguments.contains("[Agent Bridge Pi turn protocol]"));
    assert!(arguments.contains("--approve"));
    assert!(!arguments.contains("dangerously"));
    let extension = fs::read_to_string(directory.join("pi-agent-bridge.js")).unwrap();
    assert!(extension.contains("agent_settled"));
}

// ---------------------------------------------------------------------------
// Lifecycle reliability contracts (#5, #12, #13, #15)
// ---------------------------------------------------------------------------

fn sample_completion(claim_token: &str, message: &str) -> PendingTurnCompletion {
    PendingTurnCompletion::new(
        claim_token,
        SessionEvent {
            provider: FirstPartyCli::Codex.as_str().to_owned(),
            message: message.to_owned(),
            error: None,
            provider_session_id: Some("provider-session".to_owned()),
            turn_id: Some("provider-turn".to_owned()),
            created_unix_ms: 1,
        },
        None,
    )
    .unwrap()
}

fn delivery_uncertain_failure(message: &str) -> terminal::TerminalSendFailure {
    terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!("{message}"))
}

#[test]
fn interrupted_close_converges_when_recovery_runs() {
    // A close that stopped after writing the tombstone and status but before releasing the
    // claim and the journal must converge on the next lifecycle-lock holder.
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token.clone();
    claim.retain();
    let pending = sample_completion(&claim_token, "late result");
    write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
    update_status(
        directory.path(),
        "closed",
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
    let tombstone: SessionStatus = read_json(&directory.path().join(CLOSED_STATUS_FILE)).unwrap();

    assert!(recover_pending_completion(directory.path()).unwrap());

    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(event_paths(directory.path()).unwrap().is_empty());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert_eq!(status.generation, tombstone.generation);
    assert_eq!(status.error.as_deref(), Some("closed by the maintainer"));
    let preserved: SessionStatus = read_json(&directory.path().join(CLOSED_STATUS_FILE)).unwrap();
    assert_eq!(preserved.generation, tombstone.generation);
    assert_eq!(preserved.updated_unix_ms, tombstone.updated_unix_ms);
    assert!(!recover_pending_completion(directory.path()).unwrap());
}

#[test]
fn delayed_terminal_delivery_failure_cannot_overwrite_a_replacement_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let mut stale_claim = acquire_turn_claim(directory.path()).unwrap();
    stale_claim.retain_in_place();
    record_provider_result_for_claim(
        directory.path(),
        FirstPartyCli::Codex,
        "turn A result",
        Some("codex-session".to_owned()),
        Some("codex-turn-a".to_owned()),
        Some(&stale_claim.token),
    )
    .unwrap();
    let replacement = acquire_turn_claim(directory.path()).unwrap();
    replacement.retain();
    update_status(directory.path(), "claimed", None, None).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    record_follow_up_terminal_delivery_failure(
        directory.path(),
        &mut stale_claim,
        &delivery_uncertain_failure("turn A paste timed out"),
    );

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state, "working");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn legacy_owner_without_process_identity_is_repaired_only_when_its_pid_is_dead() {
    for (pid, expect_repair) in [(std::process::id(), false), (reaped_child_pid(), true)] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        // Pre-identity owner records carry only a PID on every platform.
        write_json_atomic(
            &directory.path().join(SESSION_OWNER_FILE),
            &serde_json::json!({ "pid": pid }),
        )
        .unwrap();

        assert_eq!(
            repair_dead_native_owner(directory.path()).unwrap(),
            expect_repair
        );
        assert_eq!(
            directory.path().join(TURN_CLAIM_FILE).exists(),
            !expect_repair
        );
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(
            status.state,
            if expect_repair { "closed" } else { "working" }
        );
    }
}

/// After recovery no journal remains. Temporary files an injected fault left behind (as an
/// abrupt stop would) are best-effort records: observation must ignore them.
fn assert_journal_settled(directory: &Path) {
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    query::observe_snapshot(directory).unwrap();
}

#[test]
fn provider_completion_converges_after_a_fault_before_every_mutation() {
    // Each iteration lets the completion path perform exactly `budget` filesystem
    // mutations (journal creation included) and refuses the next one, then proves that
    // the next lifecycle-lock holder converges the session from that state.
    let mut faulted = 0;
    let mut unpublished = 0;
    let mut published = 0;
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fault");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, "working", None, None).unwrap();
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        claim.retain();
        let complete = || {
            record_provider_result_for_claim(
                &directory,
                FirstPartyCli::Codex,
                "committed result",
                Some("provider-session".to_owned()),
                Some("provider-turn".to_owned()),
                Some(&claim_token),
            )
        };

        match with_fault_budget(budget, complete) {
            Ok(()) => break,
            Err(error) => assert!(injected_fault(&error), "{error:#}"),
        }
        faulted += 1;

        let recovered = recover_pending_completion(&directory).unwrap();
        assert_journal_settled(&directory);
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        let events = event_paths(&directory).unwrap();
        if status.state == "working" {
            // The journal never reached its final path (a fault before its creation, its
            // content, or its rename), so nothing was published and the turn is still
            // owned by its claim; recovery had nothing to do.
            assert!(!recovered);
            assert!(directory.join(TURN_CLAIM_FILE).exists());
            assert!(events.is_empty());
            unpublished += 1;
            complete().unwrap();
        } else {
            published += 1;
        }
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "ready");
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        assert_journal_settled(&directory);
        let events = event_paths(&directory).unwrap();
        assert_eq!(events.len(), 1);
        let stored: SessionEvent = read_json(&events[0]).unwrap();
        assert_eq!(stored.message, "committed result");
        assert!(!recover_pending_completion(&directory).unwrap());
    }
    assert!(
        faulted >= 16,
        "only {faulted} mutation boundaries were exercised"
    );
    assert_eq!(
        unpublished, 3,
        "journal creation, content, and rename boundaries were not all exercised"
    );
    assert!(
        published >= 13,
        "post-journal boundaries were not exercised"
    );
}

fn close_test_terminal() -> terminal::TerminalSession {
    terminal::TerminalSession {
        kind: if cfg!(windows) {
            terminal::TerminalKind::WindowsConsole
        } else {
            terminal::TerminalKind::AppleTerminal
        },
        id: "4242".to_owned(),
        tab_id: None,
        window_id: None,
        managed_session_id: Some("session-close123".to_owned()),
        windows_process_identity: None,
    }
}

#[test]
fn explicit_close_converges_after_a_fault_before_every_close_mutation() {
    for journaled_event in [
        JournaledEventState::Absent,
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        explicit_close_converges_after_every_fault_with(journaled_event);
    }
}

fn explicit_close_converges_after_every_fault_with(journaled_event: JournaledEventState) {
    let mut faulted = 0;
    let mut tombstoned = 0;
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_close_fixture(journaled_event);
        let CloseFixture {
            directory,
            request_id,
            pending,
            event_path,
            ..
        } = &fixture;
        let directory = directory.as_path();
        let mut adapter_calls = 0;

        let outcome = with_fault_budget(budget, || {
            close_session_state_with_error(
                directory,
                Some("closed by the maintainer".to_owned()),
                |_| {
                    adapter_calls += 1;
                    Ok(terminal::CloseOutcome::Closed)
                },
            )
        });
        match outcome {
            Ok(()) => break,
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
            }
        }
        faulted += 1;

        // Recovery under the lifecycle lock must finish an interrupted close whose
        // tombstone exists and must never leave a journal behind. Without a tombstone the
        // close never committed, so recovery treats the journal as an ordinary interrupted
        // completion: it publishes a matching or missing event and refuses a different one.
        let tombstoned_before_recovery = directory.join(CLOSED_STATUS_FILE).exists();
        let recovered = recover_pending_completion(directory);
        if tombstoned_before_recovery {
            tombstoned += 1;
            recovered.unwrap();
            assert_journal_settled(directory);
            let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
            assert_eq!(status.state, "closed");
            assert_eq!(status.error.as_deref(), Some("closed by the maintainer"));
            assert!(!directory.join(TURN_CLAIM_FILE).exists());
            assert!(!directory.join(LEGACY_RESUME_PENDING_FILE).exists());
        } else if journaled_event == JournaledEventState::Mismatched {
            assert!(format!("{:#}", recovered.unwrap_err()).contains("contains different data"));
        } else {
            recovered.unwrap();
            assert_journal_settled(directory);
        }

        close_session_state(directory, |_| {
            adapter_calls += 1;
            Ok(terminal::CloseOutcome::Closed)
        })
        .unwrap();
        assert!(
            adapter_calls <= 2,
            "the terminal adapter ran {adapter_calls} times"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "closed");
        let tombstone: SessionStatus = read_json(&directory.join(CLOSED_STATUS_FILE)).unwrap();
        assert_eq!(tombstone.generation, status.generation);
        assert!(!directory.join(TURN_CLAIM_FILE).exists());
        assert!(!directory.join(TURN_COMPLETION_FILE).exists());
        assert!(!directory.join(LEGACY_RESUME_PENDING_FILE).exists());
        assert!(!directory.join(TERMINAL_HANDLE_FILE).exists());
        assert!(!directory.join(TERMINAL_CLOSING_FILE).exists());
        assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
        assert!(!recover_pending_completion(directory).unwrap());
        // Once the close committed, its commit point wins: a journal whose event was never
        // written is discarded. An event the completion already wrote is the provider's
        // result and stays published exactly when it matches its journal. A close that
        // never committed lets recovery publish the completion first.
        let (request, session) = request_state(directory, request_id);
        assert_eq!(session, "closed");
        let published = match journaled_event {
            JournaledEventState::Committed => true,
            JournaledEventState::Absent => !tombstoned_before_recovery,
            JournaledEventState::Mismatched | JournaledEventState::Oversized(_) => false,
        };
        assert_eq!(
            request,
            if published { "completed" } else { "unresolved" },
            "{journaled_event:?} budget {budget}"
        );
        assert_eq!(
            event_paths(directory).unwrap(),
            if published {
                vec![event_path.clone()]
            } else {
                Vec::new()
            }
        );
        let quarantined = directory
            .join("events")
            .join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
        assert_eq!(
            quarantined.exists(),
            journaled_event == JournaledEventState::Mismatched
        );
    }
    assert!(
        faulted >= 20,
        "only {faulted} close mutation boundaries were exercised for {journaled_event:?}"
    );
    assert!(
        tombstoned >= 8,
        "interrupted post-tombstone cleanup was not exercised for {journaled_event:?}"
    );
    // The terminal handle is claimed by a direct rename with its own sync; both of that
    // pair's boundaries are fault points like every other rename's.
    for label in [
        "claiming the terminal handle for close",
        "syncing the claimed terminal handle's directory",
    ] {
        assert!(
            labels.iter().any(|error| error.contains(label)),
            "no fault before {label} for {journaled_event:?}: {labels:?}"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TurnEvent {
    HookCompletion,
    MonitorFailure,
    ProcessExit,
    DelayedDeliveryFailure,
}

fn permutations<T: Copy>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut all = Vec::new();
    for (index, first) in items.iter().enumerate() {
        let mut rest = items.to_vec();
        rest.remove(index);
        for mut tail in permutations(&rest) {
            tail.insert(0, *first);
            all.push(tail);
        }
    }
    all
}

#[test]
fn turn_lifecycle_converges_under_every_completion_exit_and_failure_order() {
    let events = [
        TurnEvent::HookCompletion,
        TurnEvent::MonitorFailure,
        TurnEvent::ProcessExit,
        TurnEvent::DelayedDeliveryFailure,
    ];
    let orders = permutations(&events);
    assert_eq!(orders.len(), 24);
    for order in orders {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "launching", None, None).unwrap();
        update_status(directory.path(), "running", None, None).unwrap();
        update_status(directory.path(), "ready", None, None).unwrap();
        let mut claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain_in_place();
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let mut previous: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

        for event in &order {
            let claim_present = directory.path().join(TURN_CLAIM_FILE).exists();
            match event {
                TurnEvent::HookCompletion => record_provider_result_for_claim(
                    directory.path(),
                    FirstPartyCli::Codex,
                    "hook result",
                    Some("codex-session".to_owned()),
                    Some("codex-turn".to_owned()),
                    Some(&claim.token),
                )
                .unwrap(),
                TurnEvent::MonitorFailure => record_provider_monitor_failure(
                    directory.path(),
                    FirstPartyCli::Codex,
                    "monitor stopped",
                )
                .unwrap(),
                TurnEvent::ProcessExit => {
                    finalize_native_session(directory.path(), &Ok(())).unwrap()
                }
                TurnEvent::DelayedDeliveryFailure => record_follow_up_terminal_delivery_failure(
                    directory.path(),
                    &mut claim,
                    &delivery_uncertain_failure("paste timed out"),
                ),
            }
            let current: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert!(
                valid_status_transition(&previous.state, &current.state),
                "{order:?}: {event:?} moved {} -> {}",
                previous.state,
                current.state
            );
            assert!(
                current.generation >= previous.generation,
                "{order:?}: {event:?}"
            );
            if *event == TurnEvent::DelayedDeliveryFailure && !claim_present {
                assert_eq!(current.generation, previous.generation, "{order:?}");
                assert_eq!(current.error, previous.error, "{order:?}");
            }
            assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
            previous = current;
        }

        assert!(
            !directory.path().join(TURN_CLAIM_FILE).exists(),
            "{order:?}"
        );
        match previous.state.as_str() {
            "exited" => assert_eq!(previous.error, None, "{order:?}"),
            "failed" => assert_eq!(
                previous.error.as_deref(),
                Some("monitor stopped"),
                "{order:?}"
            ),
            other => panic!("{order:?} ended in {other}"),
        }
        let stored = event_paths(directory.path())
            .unwrap()
            .iter()
            .map(|path| read_json::<SessionEvent>(path).unwrap())
            .collect::<Vec<_>>();
        assert!(stored.len() <= 2, "{order:?}: {stored:?}");
        assert!(
            stored
                .iter()
                .filter(|event| event.message == "hook result")
                .count()
                <= 1,
            "{order:?}"
        );
        assert!(
            stored
                .iter()
                .all(|event| event.error.as_deref() != Some("paste timed out")),
            "{order:?}"
        );
    }
}

#[test]
fn result_wait_repairs_a_dead_owner_and_ends_the_wait() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token.clone();
    claim.retain();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: reaped_child_pid(),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();

    let started = Instant::now();
    let error = wait_for_event_for_turn(
        directory.path(),
        0,
        None,
        Some(&claim_token),
        Duration::from_secs(30),
    )
    .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        format!("{error:#}").contains("no longer running"),
        "{error:#}"
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
}

#[test]
fn atomic_json_write_syncs_temporary_then_persisted_file_then_parent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.json");
    let (outcome, log) = with_sync_log(|| write_json_atomic(&path, &serde_json::json!({})));
    outcome.unwrap();

    assert_eq!(log.len(), 3, "{log:?}");
    match &log[0] {
        SyncRecord::File(temporary) => {
            assert_eq!(temporary.parent(), Some(directory.path()));
            assert_ne!(temporary, &path);
            assert!(temporary.extension().is_some_and(|ext| ext == "tmp"));
        }
        other => panic!("first sync was {other:?}"),
    }
    assert_eq!(log[1], SyncRecord::File(path.clone()));
    assert_eq!(
        log[2],
        SyncRecord::Directory(directory.path().to_path_buf())
    );
}

#[test]
fn private_write_claim_creation_and_removal_sync_file_then_parent() {
    let directory = tempfile::tempdir().unwrap();
    let private = directory.path().join("initial-prompt.txt");
    let (outcome, log) = with_sync_log(|| write_private(&private, b"prompt"));
    outcome.unwrap();
    assert_eq!(
        log,
        [
            SyncRecord::File(private.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );

    let claim_path = directory.path().join(TURN_CLAIM_FILE);
    let (outcome, log) = with_sync_log(|| acquire_turn_claim(directory.path()));
    outcome.unwrap().retain();
    assert_eq!(
        log[..2],
        [
            SyncRecord::File(claim_path.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );

    let (outcome, log) = with_sync_log(|| remove_file_if_present(&private));
    outcome.unwrap();
    assert_eq!(log, [SyncRecord::Directory(directory.path().to_path_buf())]);
    let (outcome, log) = with_sync_log(|| remove_file_if_present(&private));
    outcome.unwrap();
    assert!(
        log.is_empty(),
        "a missing file must not be reported as synced"
    );
}

#[test]
fn session_directory_creation_syncs_the_state_root_before_its_records() {
    let root = tempfile::tempdir().unwrap();
    // The root's parent is taken as durable, so the ancestry walk syncs it once and stops.
    let durable = [root.path().parent().unwrap().to_path_buf()];
    let (outcome, log) = with_sync_log(|| {
        create_session_within(
            root.path(),
            &durable,
            SessionSpec {
                provider: FirstPartyCli::Codex,
                provider_path: PathBuf::from("codex"),
                provider_version: "0.147.0".to_owned(),
                workspace: root.path().to_path_buf(),
                title: "durability".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "prompt".to_owned(),
            },
        )
    });
    let created = outcome.unwrap();

    // The root's own entry, then its durability receipt (which syncs the root), then the
    // root again for the session entry, then the session directory, before any record.
    assert_eq!(
        log.first(),
        Some(&SyncRecord::Directory(durable[0].clone()))
    );
    let receipt_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(root.path().join(STATE_ROOT_DURABLE_FILE)))
        .expect("receipt sync");
    assert_eq!(
        log[receipt_index + 1..receipt_index + 4],
        [
            SyncRecord::Directory(root.path().to_path_buf()),
            SyncRecord::Directory(root.path().to_path_buf()),
            SyncRecord::Directory(created.directory.clone()),
        ],
        "{log:?}"
    );
    assert!(created.directory.join("events").is_dir());
    let manifest_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(created.directory.join("manifest.json")))
        .expect("manifest sync");
    assert!(manifest_index > receipt_index + 3);
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.state, "launching");
    assert_eq!(status.error, None);
}

// ---------------------------------------------------------------------------
// Review round 2: event-written partial completions, initial delivery writer,
// state-root ancestors, fault and sync coverage
// ---------------------------------------------------------------------------

fn write_test_manifest(directory: &Path) {
    let id = directory.file_name().unwrap().to_str().unwrap().to_owned();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id,
            provider: "codex".to_owned(),
            provider_path: PathBuf::from("/opt/codex"),
            provider_version: "codex-cli 0.147.0".to_owned(),
            workspace: directory.to_path_buf(),
            title: "review round 2".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        },
    )
    .unwrap();
}

/// A completion that stopped right after writing its event: journal and event exist, the
/// claim is still held, and the status was never updated. Returns the request address and
/// the journal's event path. `event_message` lets the written event disagree with the journal.
fn seed_event_written_completion(directory: &Path, event_message: &str) -> (String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let mut pending = sample_completion(&claim.token, "late result");
    pending.event_file = claim.receipt.event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}

fn request_state(directory: &Path, request_id: &str) -> (String, String) {
    let value = query::request_result(directory, request_id).unwrap();
    (
        value["request_state"].as_str().unwrap().to_owned(),
        value["session_state"].as_str().unwrap().to_owned(),
    )
}

#[test]
fn interrupted_close_publishes_a_journaled_completion_whose_event_was_written() {
    // Completion wrote journal and event, then the close wrote its tombstone and stopped.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-review2");
    let (request_id, event_path) = seed_event_written_completion(&directory, "late result");
    update_status(&directory, "closed", None, Some("closed".to_owned())).unwrap();
    let tombstone: SessionStatus = read_json(&directory.join(CLOSED_STATUS_FILE)).unwrap();

    let before = request_state(&directory, &request_id);
    assert!(recover_pending_completion(&directory).unwrap());
    let after = request_state(&directory, &request_id);

    assert_eq!(before, ("completed".to_owned(), "closed".to_owned()));
    assert_eq!(after, before);
    assert!(event_path.exists());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert_eq!(status.generation, tombstone.generation);
    assert!(!recover_pending_completion(&directory).unwrap());
}

#[test]
fn close_cleanup_publishes_a_journaled_completion_whose_event_was_written() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-review2");
    let (request_id, event_path) = seed_event_written_completion(&directory, "late result");

    let before = request_state(&directory, &request_id);
    close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    let after = request_state(&directory, &request_id);

    assert_eq!(before.0, "completed");
    assert_eq!(after, ("completed".to_owned(), "closed".to_owned()));
    assert!(event_path.exists());
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
}

#[test]
fn close_keeps_but_never_publishes_an_event_that_disagrees_with_its_journal() {
    for interrupted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-review2");
        let (request_id, event_path) =
            seed_event_written_completion(&directory, "different result");
        if interrupted {
            update_status(&directory, "closed", None, None).unwrap();
        }

        let before = request_state(&directory, &request_id);
        if interrupted {
            assert!(recover_pending_completion(&directory).unwrap());
        } else {
            close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
        }
        let after = request_state(&directory, &request_id);

        assert_eq!(before.0, "recovery_required", "interrupted={interrupted}");
        assert_eq!(
            after,
            ("unresolved".to_owned(), "closed".to_owned()),
            "interrupted={interrupted}"
        );
        assert!(!event_path.exists());
        let quarantined = directory.join("events").join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            event_path.file_name().unwrap().to_str().unwrap()
        ));
        let kept: SessionEvent = read_json(&quarantined).unwrap();
        assert_eq!(kept.message, "different result");
        assert!(event_paths(&directory).unwrap().is_empty());
        assert!(!directory.join(TURN_COMPLETION_FILE).exists());
        assert!(!recover_pending_completion(&directory).unwrap());
    }
}

#[test]
fn late_initial_cross_session_uncertainty_cannot_write_into_a_newer_turn() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let mut initial = acquire_turn_claim(directory.path()).unwrap();
    initial.retain_in_place();
    // The target completed the initial turn and a tell claimed the replacement turn before
    // the initial messenger reported that its delivery could not be confirmed.
    record_provider_result_for_claim(
        directory.path(),
        FirstPartyCli::Claude,
        "initial result",
        None,
        Some("claude-turn-1".to_owned()),
        Some(&initial.token),
    )
    .unwrap();
    let (replacement, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(before.state, "claimed");

    record_cross_session_delivery_uncertainty(
        directory.path(),
        &mut initial,
        &anyhow::anyhow!("late initial report"),
    );
    drop(initial);

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state, "claimed");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert_eq!(
        current_turn_claim_token(directory.path()).unwrap(),
        Some(replacement.token.clone())
    );
    replacement.retain();
}

#[test]
fn initial_cross_session_uncertainty_keeps_its_own_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let mut initial = acquire_turn_claim(directory.path()).unwrap();

    record_cross_session_delivery_uncertainty(
        directory.path(),
        &mut initial,
        &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
    );
    let token = initial.token.clone();
    drop(initial);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state, "working");
    assert_eq!(
        status.error.as_deref(),
        Some("delivery unconfirmed: executed input was not reported")
    );
    assert_eq!(
        current_turn_claim_token(directory.path()).unwrap(),
        Some(token)
    );
}

/// A state root two levels below a pre-existing base directory, with the base taken as
/// durable so the ancestry walk stops there.
struct NestedStateRoot {
    base: tempfile::TempDir,
    ancestor: PathBuf,
    root: PathBuf,
}

impl NestedStateRoot {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let ancestor = base.path().join("custom");
        let root = ancestor.join("native-sessions");
        Self {
            base,
            ancestor,
            root,
        }
    }

    fn spec(&self) -> SessionSpec {
        SessionSpec {
            provider: FirstPartyCli::Codex,
            provider_path: PathBuf::from("codex"),
            provider_version: "0.147.0".to_owned(),
            workspace: self.base.path().to_path_buf(),
            title: "durability".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            prompt: "prompt".to_owned(),
        }
    }

    fn durable(&self) -> Vec<PathBuf> {
        vec![self.base.path().to_path_buf()]
    }

    fn create(&self) -> Result<CreatedSession> {
        create_session_within(&self.root, &self.durable(), self.spec())
    }

    fn receipt(&self) -> PathBuf {
        self.root.join(STATE_ROOT_DURABLE_FILE)
    }

    fn receipt_present(&self) -> bool {
        state_root_durability_receipt_present(&self.root)
    }

    /// The sync log of a creation that establishes the ancestry: the root's entry in its
    /// parent, the parent's entry in the base, the receipt (a temporary file, the receipt
    /// itself, then the root), the root again for the session entry, then the session.
    fn assert_established_ancestry(&self, log: &[SyncRecord], session: &Path) {
        assert_eq!(
            log[..2],
            [
                SyncRecord::Directory(self.ancestor.clone()),
                SyncRecord::Directory(self.base.path().to_path_buf()),
            ],
            "{log:?}"
        );
        assert!(
            matches!(&log[2], SyncRecord::File(temporary) if temporary.parent() == Some(self.root.as_path())),
            "{log:?}"
        );
        assert_eq!(
            log[3..7],
            [
                SyncRecord::File(self.receipt()),
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(session.to_path_buf()),
            ],
            "{log:?}"
        );
    }

    /// The sync log of a creation that found the receipt: the root's entry for the new
    /// session directory only, nothing above the root and no receipt.
    fn assert_trusted_receipt(&self, log: &[SyncRecord], session: &Path) {
        assert_eq!(
            log[..2],
            [
                SyncRecord::Directory(self.root.clone()),
                SyncRecord::Directory(session.to_path_buf()),
            ],
            "{log:?}"
        );
        assert!(
            !log.contains(&SyncRecord::Directory(self.ancestor.clone())),
            "{log:?}"
        );
        assert!(
            !log.contains(&SyncRecord::Directory(self.base.path().to_path_buf())),
            "{log:?}"
        );
        assert!(!log.contains(&SyncRecord::File(self.receipt())), "{log:?}");
    }
}

#[test]
fn session_directory_creation_syncs_newly_created_state_root_ancestors() {
    let nested = NestedStateRoot::new();
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();

    // Nearest entry first: the root's entry in its parent, then the parent's entry in the
    // pre-existing base, then the receipt, then the root itself for the session entry.
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(created.directory.join("events").is_dir());
    assert!(nested.receipt_present());

    // A root with its receipt syncs nothing above itself.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_trusted_receipt(&log, &created.directory);
}

// ---------------------------------------------------------------------------
// Review round 8: state-root ancestry durability independent of who created it
// ---------------------------------------------------------------------------

const STATE_ROOT_ANCESTRY_SYNC_LABEL: &str = "syncing the state root's ancestry";

#[test]
fn a_later_creator_establishes_the_ancestry_its_stopped_creator_left_unsynced() {
    // Creator A stops right before the ancestry syncs: the root and its ancestor exist,
    // nothing was synced, and no receipt claims otherwise.
    let nested = NestedStateRoot::new();
    let (outcome, log) = with_sync_log(|| with_fault_budget(0, || nested.create()));
    let error = outcome.err().expect("creator A stopped");
    assert!(injected_fault(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains(STATE_ROOT_ANCESTRY_SYNC_LABEL),
        "{error:#}"
    );
    assert!(log.is_empty(), "{log:?}");
    assert!(nested.root.is_dir());
    assert!(!nested.receipt_present());

    // Creator B finds an existing root and still performs every ancestry sync before it
    // writes the receipt and its own session.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!((status.state.as_str(), status.error), ("launching", None));

    // With the receipt present, creator C syncs only the root entry for its session.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_trusted_receipt(&log, &created.directory);

    // Every stop of creator A: the receipt exists only after both ancestry syncs, and a
    // creator that finds no receipt redoes the walk however far A got.
    for budget in 0.. {
        let nested = NestedStateRoot::new();
        let (outcome, a_log) = with_sync_log(|| with_fault_budget(budget, || nested.create()));
        let receipt_after_a = nested.receipt_present();
        // A stop right after the receipt's rename leaves an unsynced receipt behind, which
        // is safe: both ancestry syncs precede the first write under the root, so a
        // receipt that survives never outlives the durability it attests to.
        if receipt_after_a {
            assert_eq!(
                a_log[..2],
                [
                    SyncRecord::Directory(nested.ancestor.clone()),
                    SyncRecord::Directory(nested.base.path().to_path_buf()),
                ],
                "budget {budget}: the receipt was written before the ancestry syncs: {a_log:?}"
            );
        }
        let (outcome_b, b_log) = with_sync_log(|| nested.create());
        let created_b = outcome_b.unwrap();
        assert!(nested.receipt_present(), "budget {budget}");
        if receipt_after_a {
            nested.assert_trusted_receipt(&b_log, &created_b.directory);
        } else {
            nested.assert_established_ancestry(&b_log, &created_b.directory);
        }
        match outcome {
            Ok(_) => {
                assert!(receipt_after_a, "budget {budget}");
                break;
            }
            Err(error) => assert!(injected_fault(&error), "budget {budget}: {error:#}"),
        }
    }
}

#[test]
fn the_ancestry_walk_covers_ancestors_this_creator_did_not_make() {
    // Another creator made the ancestor and stopped before making the root or syncing
    // anything. This creator makes only the root, yet the ancestor's entry in the base is
    // synced as well: the walk is over the root's ancestry, not over what was created here.
    let nested = NestedStateRoot::new();
    fs::create_dir(&nested.ancestor).unwrap();
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
}

#[test]
fn the_ancestry_walk_is_bounded() {
    let base = tempfile::tempdir().unwrap();
    let mut levels = Vec::new();
    let mut root = base.path().to_path_buf();
    for level in 1..=18 {
        root = root.join(format!("l{level}"));
        levels.push(root.clone());
    }
    root = root.join("native-sessions");
    let durable = [base.path().to_path_buf()];
    let (outcome, log) = with_sync_log(|| {
        create_session_within(
            &root,
            &durable,
            SessionSpec {
                provider: FirstPartyCli::Codex,
                provider_path: PathBuf::from("codex"),
                provider_version: "0.147.0".to_owned(),
                workspace: base.path().to_path_buf(),
                title: "durability".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                prompt: "prompt".to_owned(),
            },
        )
    });
    outcome.unwrap();
    let receipt_index = log
        .iter()
        .position(|record| *record == SyncRecord::File(root.join(STATE_ROOT_DURABLE_FILE)))
        .expect("receipt sync");
    let walked: Vec<&SyncRecord> = log[..receipt_index]
        .iter()
        .filter(|record| matches!(record, SyncRecord::Directory(_)))
        .collect();
    // The nearest sixteen holders: l18 (the root's entry) down to l3; l2, l1, and the
    // base are beyond the bound.
    let expected: Vec<SyncRecord> = levels
        .iter()
        .rev()
        .take(STATE_ROOT_ANCESTRY_SYNC_LIMIT)
        .map(|level| SyncRecord::Directory(level.clone()))
        .collect();
    assert_eq!(walked.len(), STATE_ROOT_ANCESTRY_SYNC_LIMIT, "{log:?}");
    assert!(
        walked
            .iter()
            .zip(&expected)
            .all(|(walked, expected)| *walked == expected),
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::Directory(levels[1].clone())),
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::Directory(base.path().to_path_buf())),
        "{log:?}"
    );
    assert!(state_root_durability_receipt_present(&root));
}

#[test]
fn an_ancestry_sync_failure_never_blocks_session_creation_and_leaves_no_receipt() {
    let nested = NestedStateRoot::new();
    let (outcome, log) =
        with_sync_failure(nested.base.path(), || with_sync_log(|| nested.create()));
    let created = outcome.unwrap();
    // Both syncs were attempted, in order; the failure stopped the walk before the receipt.
    assert_eq!(
        log[..2],
        [
            SyncRecord::Directory(nested.ancestor.clone()),
            SyncRecord::Directory(nested.base.path().to_path_buf()),
        ],
        "{log:?}"
    );
    assert!(
        !log.contains(&SyncRecord::File(nested.receipt())),
        "{log:?}"
    );
    assert_eq!(
        log[2],
        SyncRecord::Directory(nested.root.clone()),
        "{log:?}"
    );
    assert!(!nested.receipt_present());
    assert!(created.directory.join("manifest.json").is_file());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.state, "launching");
    let error = status
        .error
        .expect("the launch status records the failed walk");
    assert!(
        error.contains("state root ancestry was not made durable"),
        "{error}"
    );
    assert!(error.contains("injected sync failure for"), "{error}");
    assert!(
        error.contains(&nested.base.path().display().to_string()),
        "{error}"
    );

    // The next creation walks again, and this time writes the receipt.
    let (outcome, log) = with_sync_log(|| nested.create());
    let created = outcome.unwrap();
    nested.assert_established_ancestry(&log, &created.directory);
    assert!(nested.receipt_present());
    let status: SessionStatus = read_json(&created.directory.join("status.json")).unwrap();
    assert_eq!(status.error, None);
}

#[test]
fn a_non_regular_receipt_is_not_trusted() {
    // A directory at the receipt's path proves nothing; the walk runs, and the receipt
    // write then fails loudly rather than silently accepting the impostor.
    let nested = NestedStateRoot::new();
    fs::create_dir_all(nested.receipt()).unwrap();
    let (outcome, log) = with_sync_log(|| nested.create());
    let error = outcome.err().expect("the receipt write fails");
    assert!(!injected_sync_failure(&error), "{error:#}");
    assert_eq!(
        log[..2],
        [
            SyncRecord::Directory(nested.ancestor.clone()),
            SyncRecord::Directory(nested.base.path().to_path_buf()),
        ],
        "{log:?}"
    );
    assert!(!nested.receipt_present());
}

#[test]
fn record_rename_syncs_the_destination_directory_then_a_different_source_directory() {
    let directory = tempfile::tempdir().unwrap();
    let from = directory.path().join("terminal.json");
    let to = directory.path().join("terminal.closing.json");
    fs::write(&from, "{}").unwrap();
    let (outcome, log) = with_sync_log(|| rename_session_file(&from, &to));
    outcome.unwrap();
    assert_eq!(log, [SyncRecord::Directory(directory.path().to_path_buf())]);

    let other = directory.path().join("events");
    fs::create_dir(&other).unwrap();
    let moved = other.join("terminal.closing.json");
    let (outcome, log) = with_sync_log(|| rename_session_file(&to, &moved));
    outcome.unwrap();
    assert_eq!(
        log,
        [
            SyncRecord::Directory(other.clone()),
            SyncRecord::Directory(directory.path().to_path_buf()),
        ]
    );
    assert!(moved.exists());
}

// ---------------------------------------------------------------------------
// Review round 3: publication evidence through close, budgeted publication reads,
// one byte-match predicate, terminal-handle fault boundary
// ---------------------------------------------------------------------------

struct CloseFixture {
    root: tempfile::TempDir,
    directory: PathBuf,
    request_id: String,
    pending: PendingTurnCompletion,
    event_path: PathBuf,
}

/// A working session under `session-fault` with a held claim, a journaled completion
/// whose event is absent, committed, or mismatched, a terminal handle, and a legacy
/// resume marker: everything an explicit close has to settle.
fn seed_close_fixture(journaled_event: JournaledEventState) -> CloseFixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-fault");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let mut pending = sample_completion(&claim.token, "late result");
    pending.event_file = claim.receipt.event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    match journaled_event {
        JournaledEventState::Absent => {}
        JournaledEventState::Committed => {
            write_json_atomic(&event_path, &pending.event).unwrap();
        }
        JournaledEventState::Mismatched => {
            let mut event = pending.event.clone();
            event.message = "different result".to_owned();
            write_json_atomic(&event_path, &event).unwrap();
        }
        JournaledEventState::Oversized(_) => unreachable!("fixtures never exceed the read limit"),
    }
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &close_test_terminal(),
    )
    .unwrap();
    fs::write(directory.join(LEGACY_RESUME_PENDING_FILE), "{}").unwrap();
    CloseFixture {
        root,
        directory,
        request_id,
        pending,
        event_path,
    }
}

// The query commands exactly as the CLI runs them, parsed from argv and evaluated over
// an explicit state root instead of the process environment.
fn cli_result(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Result(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a result command")
    };
    query::result_value_in(root, &request).unwrap()
}

fn cli_search(root: &Path, args: &[&str]) -> serde_json::Value {
    let NativeCommand::Search(request) = parse_args(args).unwrap() else {
        panic!("{args:?} is not a search command")
    };
    query::search_value_in(root, &request).unwrap()
}

fn cli_sessions(root: &Path) -> Vec<serde_json::Value> {
    let NativeCommand::Sessions(request) = parse_args(["sessions", "--json"]).unwrap() else {
        panic!("not a sessions command")
    };
    sessions_in(root, &request).unwrap()
}

/// `--context-result <address>` resolution as `ask` and `tell` perform it: the prompt the
/// target would receive, or the resolution error.
fn cli_context_result(root: &Path, address: &str) -> Result<String> {
    let mut references = Vec::new();
    context::push_option(&mut references, address)?;
    Ok(context::resolve_in(root, &references)?.prompt_with_attachments("continue"))
}

#[test]
fn every_close_boundary_reports_a_committed_result_before_and_after_sessions() {
    for journaled_event in [
        JournaledEventState::Committed,
        JournaledEventState::Mismatched,
    ] {
        close_boundary_queries_agree_with(journaled_event);
    }
}

/// Stops the close before every filesystem mutation and, without recovering first, runs
/// `result`, `result --wait`, `search`, and a `--context-result` resolution over the
/// interrupted records. A committed event is reported as `completed` at every boundary,
/// before and after `sessions` converges the close; a mismatched event never is.
fn close_boundary_queries_agree_with(journaled_event: JournaledEventState) {
    let committed = journaled_event == JournaledEventState::Committed;
    let mut faulted = 0;
    let mut claim_released_before_journal = 0;
    for budget in 0.. {
        let fixture = seed_close_fixture(journaled_event);
        let root = fixture.root.path();
        let outcome = with_fault_budget(budget, || {
            close_session_state_with_error(
                &fixture.directory,
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed),
            )
        });
        match outcome {
            Ok(()) => break,
            Err(error) => assert!(injected_fault(&error), "{error:#}"),
        }
        faulted += 1;
        let label = format!("{journaled_event:?} budget {budget}");

        // The claim is released before the journal is removed, so no boundary leaves a
        // claim that hides a committed event without the journal that proves it.
        let claim_present = fixture.directory.join(TURN_CLAIM_FILE).exists();
        let journal_present = fixture.directory.join(TURN_COMPLETION_FILE).exists();
        assert!(
            journal_present || !claim_present,
            "{label}: the claim outlived the completion journal"
        );
        if journal_present && !claim_present {
            claim_released_before_journal += 1;
        }
        let tombstoned = fixture.directory.join(CLOSED_STATUS_FILE).exists();

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} before"));

        let listing = cli_sessions(root);
        assert_eq!(listing.len(), 1, "{label}");
        assert_eq!(listing[0]["id"], "session-fault");
        // Without a tombstone the close never committed: `sessions` recovers the journal
        // as an ordinary completion instead, which publishes a committed event and
        // refuses a mismatched one.
        let expected_session_state = match (tombstoned, committed) {
            (true, _) => "closed",
            (false, true) => "ready",
            (false, false) => "working",
        };
        assert_eq!(listing[0]["state"], expected_session_state, "{label}");

        assert_close_boundary_queries(root, &fixture, committed, &format!("{label} after"));
        let after = cli_result(
            root,
            &[
                "result",
                "session-fault",
                "--request",
                &fixture.request_id,
                "--json",
            ],
        );
        assert_eq!(after["session_state"], expected_session_state, "{label}");
        assert_eq!(
            after["recovery_required"],
            !tombstoned && !committed,
            "{label}"
        );
        if tombstoned {
            assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists(), "{label}");
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label}"
            );
        }
    }
    assert!(
        faulted >= 20,
        "only {faulted} close boundaries were exercised for {journaled_event:?}"
    );
    assert!(
        claim_released_before_journal >= 2,
        "the boundaries between claim release and journal removal were not exercised for {journaled_event:?}"
    );
}

fn assert_close_boundary_queries(
    root: &Path,
    fixture: &CloseFixture,
    committed: bool,
    label: &str,
) {
    let id = "session-fault";
    let request_id = fixture.request_id.as_str();
    let plain = cli_result(root, &["result", id, "--request", request_id, "--json"]);
    let waited = cli_result(
        root,
        &[
            "result",
            id,
            "--request",
            request_id,
            "--wait",
            "--timeout-secs",
            "1",
            "--json",
        ],
    );
    let search = cli_search(
        root,
        &["search", "late result", "--all-workspaces", "--json"],
    );
    let context = cli_context_result(root, &format!("{id}/{request_id}"));
    if committed {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert_eq!(value["ok"], true, "{label}: {name}: {value}");
            assert_eq!(
                value["request_state"], "completed",
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], "late result", "{label}: {name}");
            assert_eq!(
                value["event_id"], fixture.pending.event_file,
                "{label}: {name}"
            );
            assert_eq!(
                value["timed_out"],
                serde_json::Value::Null,
                "{label}: {name}"
            );
        }
        let hits = search["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{label}: {search}");
        assert_eq!(hits[0]["session"], id, "{label}");
        assert_eq!(hits[0]["request_id"], request_id, "{label}");
        assert_eq!(hits[0]["event_id"], fixture.pending.event_file, "{label}");
        assert_eq!(search["incomplete"], false, "{label}: {search}");
        assert_eq!(search["scanned"]["events"], 1, "{label}: {search}");
        let prompt = context.unwrap_or_else(|error| panic!("{label}: {error:#}"));
        assert!(prompt.contains("late result"), "{label}: {prompt}");
    } else {
        for (name, value) in [("result", &plain), ("result --wait", &waited)] {
            assert!(
                matches!(
                    value["request_state"].as_str(),
                    Some("recovery_required" | "unresolved")
                ),
                "{label}: {name}: {value}"
            );
            assert_eq!(value["result"], serde_json::Value::Null, "{label}: {name}");
        }
        assert!(
            search["hits"].as_array().unwrap().is_empty(),
            "{label}: {search}"
        );
        // While the journal and the differing event are both still in place, the scan
        // read the event to decide publication, skipped it, and says so.
        let unverified_in_place =
            fixture.directory.join(TURN_COMPLETION_FILE).exists() && fixture.event_path.exists();
        let reasons = search["incomplete_reasons"].to_string();
        assert_eq!(
            reasons.contains("differs from its pending completion journal"),
            unverified_in_place,
            "{label}: {reasons}"
        );
        assert_eq!(
            search["incomplete"], unverified_in_place,
            "{label}: {search}"
        );
        assert_eq!(
            search["scanned"]["events"],
            u64::from(unverified_in_place),
            "{label}: {search}"
        );
        let error = context.unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot be attached"),
            "{label}: {error:#}"
        );
    }
}

/// Like [`seed_event_written_completion`], with the journal's own message chosen too.
fn seed_event_written_completion_with(
    directory: &Path,
    journal_message: &str,
    event_message: &str,
) -> (String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let mut pending = sample_completion(&claim.token, journal_message);
    pending.event_file = claim.receipt.event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}

#[test]
fn search_charges_publication_reads_against_its_byte_budget() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-budget");
    let message = format!("needle {}", "x".repeat(4200));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    assert!(size > 4200);
    let search = |max_bytes: Option<u64>| {
        let mut args = vec!["search", "needle", "--all-workspaces", "--json"];
        let limit = max_bytes.map(|value| value.to_string());
        if let Some(limit) = &limit {
            args.extend(["--max-bytes", limit]);
        }
        cli_search(root.path(), &args)
    };

    // A one-byte budget cannot verify the journaled event's publication. The scan does
    // not read the record outside its budget: it stops and names the record instead.
    let starved = search(Some(1));
    assert!(starved["hits"].as_array().unwrap().is_empty(), "{starved}");
    assert_eq!(starved["incomplete"], true, "{starved}");
    assert_eq!(starved["scanned"]["events"], 0, "{starved}");
    let reasons = starved["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "byte budget of 1 bytes exhausted; session-budget/{event_id} is {size} bytes with 1 bytes remaining"
        )),
        "{reasons}"
    );
    let short = search(Some(size - 1));
    assert_eq!(short["incomplete"], true, "{short}");
    assert!(
        short["incomplete_reasons"]
            .to_string()
            .contains(&format!("with {} bytes remaining", size - 1)),
        "{short}"
    );

    // Exactly the event's size: the publication read consumes the whole budget and the
    // committed bytes it holds are searched without a second read.
    let exact = search(Some(size));
    let hits = exact["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{exact}");
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(exact["incomplete"], false, "{exact}");
    assert_eq!(exact["scanned"]["events"], 1, "{exact}");

    // A second session sorts after the first and holds an ordinary published hit.
    let other = root.path().join("session-other");
    fs::create_dir_all(other.join("events")).unwrap();
    write_test_manifest(&other);
    update_status(&other, "ready", None, None).unwrap();
    let mut plain = read_json::<SessionEvent>(&event_path).unwrap();
    plain.message = "needle in the other session".to_owned();
    write_json_atomic(&other.join("events").join("event-1.json"), &plain).unwrap();
    // A mismatched journaled event is read to decide publication, then skipped and
    // reported, and its bytes still count: the same budget no longer reaches the other
    // session.
    let mut differing = read_json::<SessionEvent>(&event_path).unwrap();
    differing.message = format!("needle {}", "y".repeat(4200));
    write_json_atomic(&event_path, &differing).unwrap();
    let mismatched = search(None);
    let hits = mismatched["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{mismatched}");
    assert_eq!(hits[0]["session"], "session-other");
    assert_eq!(mismatched["incomplete"], true, "{mismatched}");
    assert_eq!(mismatched["scanned"]["events"], 2, "{mismatched}");
    let reasons = mismatched["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "{event_id}: skipped; the event differs from its pending completion journal and is not published"
        )),
        "{reasons}"
    );
    let charged = search(Some(size));
    assert!(charged["hits"].as_array().unwrap().is_empty(), "{charged}");
    assert_eq!(charged["scanned"]["events"], 1, "{charged}");
    assert_eq!(charged["scanned"]["sessions"], 1, "{charged}");
    assert!(
        charged["incomplete_reasons"].to_string().contains(&format!(
            "scan stopped: byte budget of {size} bytes exhausted"
        )),
        "{charged}"
    );
}

#[test]
fn a_journaled_event_over_the_read_limit_is_never_published_or_compared() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-limit");
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, "late result", "late result");
    let size = fs::metadata(&event_path).unwrap().len();
    let pending: PendingTurnCompletion = read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
    let read = journaled_event_state_within(&directory, &pending, size).unwrap();
    assert_eq!(read.state, JournaledEventState::Committed);
    assert_eq!(read.bytes_read, size);
    assert_eq!(
        read.committed_text.as_deref(),
        Some(fs::read_to_string(&event_path).unwrap().as_str())
    );
    let read = journaled_event_state_within(&directory, &pending, size - 1).unwrap();
    assert_eq!(read.state, JournaledEventState::Oversized(size));
    assert_eq!(read.bytes_read, 0);
    assert!(read.committed_text.is_none());
    // Beyond the limit the record is not published.
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let snapshot = query::Snapshot::read_within(&directory, size - 1).unwrap();
    let value = snapshot
        .result(&directory, &query::Selector::Request(request_id.clone()))
        .unwrap();
    assert_eq!(value["request_state"], "recovery_required");
}

fn oversized_sample_event(message: &str) -> SessionEvent {
    SessionEvent {
        provider: FirstPartyCli::Codex.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id: Some("provider-session".to_owned()),
        turn_id: Some("provider-turn".to_owned()),
        created_unix_ms: 1,
    }
}

/// A working session with a held claim and no completion yet: what a provider completion
/// commits into. Returns the request address, the claim token, and the receipt's event path.
fn seed_claimed_session(directory: &Path) -> (String, String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let event_path = directory.join("events").join(&claim.receipt.event_file);
    let token = claim.token.clone();
    claim.retain();
    (request_id, token, event_path)
}

/// Everything a query or a lifecycle step can observe of a settled completion.
#[derive(Debug, PartialEq)]
struct SettledCompletion {
    request: (String, String),
    status: (String, Option<String>),
    event: SessionEvent,
    claim_present: bool,
    journal_present: bool,
}

fn settled_completion(directory: &Path, request_id: &str, event_path: &Path) -> SettledCompletion {
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    SettledCompletion {
        request: request_state(directory, request_id),
        status: (status.state, status.error),
        event: read_json(event_path).unwrap(),
        claim_present: directory.join(TURN_CLAIM_FILE).exists(),
        journal_present: directory.join(TURN_COMPLETION_FILE).exists(),
    }
}

#[test]
fn an_oversized_completion_settles_identically_whichever_write_it_stops_after() {
    // The size policy is applied when the journal is created: a completion whose event
    // record would exceed the read limit is journaled as a failed completion whose error
    // names the size. Stopping the commit before every record write, then recovering, must
    // therefore reach one final state whether the event was written before the stop or not.
    const LIMIT: u64 = 4096;
    let event = oversized_sample_event(&"x".repeat(LIMIT as usize));
    let size = serde_json::to_vec_pretty(&event).unwrap().len() as u64;
    assert!(size > LIMIT);
    let expected_error =
        format!("provider result of {size} bytes exceeds the {LIMIT} byte event limit");
    let mut journal_only = 0;
    let mut journal_and_event = 0;
    let mut settled = Vec::new();
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-size");
        let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
        let claim_path = directory.join(TURN_CLAIM_FILE);
        let outcome = with_fault_budget(budget, || {
            let _lock = lock_turn_claim(&claim_path).unwrap();
            commit_provider_completion_within_locked(
                &directory,
                &claim_path,
                &claim_token,
                event.clone(),
                None,
                "ready",
                LIMIT,
            )
        });
        let interrupted = match outcome {
            Ok(()) => false,
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                true
            }
        };
        let label = format!("budget {budget}");
        let journal_present = directory.join(TURN_COMPLETION_FILE).exists();
        if journal_present {
            // The journal never holds a publishable record over the limit.
            let pending: PendingTurnCompletion =
                read_json(&directory.join(TURN_COMPLETION_FILE)).unwrap();
            assert_eq!(pending.event.message, "", "{label}");
            assert_eq!(pending.status_state, "failed", "{label}");
            assert_eq!(
                pending.event.error.as_deref(),
                Some(expected_error.as_str()),
                "{label}"
            );
            if event_path.exists() {
                journal_and_event += 1;
            } else {
                journal_only += 1;
            }
        } else if !event_path.exists() {
            // The stop came before the journal: nothing to recover and the claim stays.
            assert!(interrupted, "{label}");
            assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
            assert_eq!(
                request_state(&directory, &request_id),
                ("pending".to_owned(), "working".to_owned()),
                "{label}"
            );
            continue;
        }
        // Otherwise the journal was created (and possibly already settled and removed):
        // recovery must converge on the one final state.
        recover_pending_completion(&directory).unwrap();
        settled.push(settled_completion(&directory, &request_id, &event_path));
        assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
        if !interrupted {
            break;
        }
    }
    assert!(
        journal_only >= 1,
        "no stop between the journal and the event"
    );
    assert!(journal_and_event >= 1, "no stop after the event");
    let first = &settled[0];
    assert_eq!(first.request, ("failed".to_owned(), "failed".to_owned()));
    assert_eq!(
        first.status,
        ("failed".to_owned(), Some(expected_error.clone()))
    );
    assert_eq!(first.event.message, "");
    assert_eq!(first.event.error.as_deref(), Some(expected_error.as_str()));
    assert_eq!(
        first.event.provider_session_id.as_deref(),
        Some("provider-session")
    );
    assert_eq!(first.event.turn_id.as_deref(), Some("provider-turn"));
    assert!(!first.claim_present);
    assert!(!first.journal_present);
    for (index, state) in settled.iter().enumerate() {
        assert_eq!(state, first, "stop {index} settled differently");
    }
}

#[test]
fn a_completion_over_the_read_limit_is_journaled_as_an_explicit_failure() {
    // The real limit: a result the publication predicate could never compare is recorded
    // as a failure that names the size, and the record it publishes stays readable.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-limit");
    let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let event = oversized_sample_event(&"x".repeat(EVENT_READ_LIMIT as usize));
    {
        let _lock = lock_turn_claim(&claim_path).unwrap();
        commit_provider_completion_locked(&directory, &claim_path, &claim_token, event, None)
            .unwrap();
    }
    let settled = settled_completion(&directory, &request_id, &event_path);
    assert_eq!(settled.request, ("failed".to_owned(), "failed".to_owned()));
    let error = settled.status.1.clone().unwrap();
    assert!(
        error.contains(&format!("exceeds the {EVENT_READ_LIMIT} byte event limit")),
        "{error}"
    );
    assert_eq!(settled.event.error.as_deref(), Some(error.as_str()));
    assert_eq!(settled.event.message, "");
    assert!(fs::metadata(&event_path).unwrap().len() < EVENT_READ_LIMIT);
    assert!(!settled.claim_present);
    assert!(!settled.journal_present);
    let value = query::request_result(&directory, &request_id).unwrap();
    assert_eq!(value["error"], error);
}

#[test]
fn a_journal_over_the_read_limit_is_never_published_whichever_write_it_stopped_after() {
    // A journal written before the size policy existed carries an event over the limit.
    // Recovery refuses to publish it whether the event file is absent or present, so the
    // interruption order cannot decide the outcome; a close then settles both the same way.
    let event = oversized_sample_event(&"x".repeat(EVENT_READ_LIMIT as usize));
    let event_bytes = serde_json::to_vec_pretty(&event).unwrap();
    let size = event_bytes.len() as u64;
    assert!(size > EVENT_READ_LIMIT);
    let mut outcomes = Vec::new();
    for event_written in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-legacy");
        let (request_id, claim_token, event_path) = seed_claimed_session(&directory);
        let mut pending = PendingTurnCompletion::new(&claim_token, event.clone(), None).unwrap();
        pending.event_file = event_path.file_name().unwrap().to_str().unwrap().to_owned();
        write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
        if event_written {
            fs::write(&event_path, &event_bytes).unwrap();
        }
        let label = format!("event_written={event_written}");

        let error = recover_pending_completion(&directory).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "{size} bytes, over the {EVENT_READ_LIMIT} byte read limit"
            )),
            "{label}: {text}"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "working", "{label}");
        assert!(directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert_eq!(event_path.exists(), event_written, "{label}");
        let before = request_state(&directory, &request_id);
        assert_eq!(before.0, "recovery_required", "{label}");

        close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
        assert!(!event_path.exists(), "{label}");
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert!(event_paths(&directory).unwrap().is_empty(), "{label}");
        outcomes.push((before, request_state(&directory, &request_id)));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(
        outcomes[0].1,
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn search_retries_a_busy_snapshot_without_spending_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-retry");
    let message = format!("needle {}", "x".repeat(4200));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    // A later session with an ordinary published hit, reached only with budget to spare.
    let other = root.path().join("session-zzz");
    fs::create_dir_all(other.join("events")).unwrap();
    write_test_manifest(&other);
    update_status(&other, "ready", None, None).unwrap();
    let mut plain = read_json::<SessionEvent>(&event_path).unwrap();
    plain.message = "needle in the other session".to_owned();
    write_json_atomic(&other.join("events").join("event-1.json"), &plain).unwrap();

    // The first snapshot of the journaled session finds a state record changed under it
    // and is retried. The snapshot reads no event, so the retry spends nothing: the
    // journaled event is read once, at its scan position, whatever the attempt count.
    // The retry window is widened so a loaded machine cannot turn the retry into a
    // busy verdict; the retry itself, not its timing, is under test.
    let attempts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let search = |max_bytes: u64, disturb: fn(&Path)| {
        let attempts = std::rc::Rc::clone(&attempts);
        attempts.set(0);
        let mut fired = false;
        query::with_snapshot_retry_window(Duration::from_secs(60), || {
            query::with_snapshot_hook(
                move |directory: &Path| {
                    attempts.set(attempts.get() + 1);
                    if !fired {
                        fired = true;
                        disturb(directory);
                    }
                },
                || {
                    let budget = max_bytes.to_string();
                    with_publication_read_log(|| {
                        cli_search(
                            root.path(),
                            &[
                                "search",
                                "needle",
                                "--all-workspaces",
                                "--max-bytes",
                                &budget,
                                "--json",
                            ],
                        )
                    })
                },
            )
        })
    };
    fn bump_status(directory: &Path) {
        update_status(directory, "working", None, None).unwrap();
    }
    fn drop_manifest(directory: &Path) {
        fs::remove_file(directory.join("manifest.json")).unwrap();
    }

    // Budget for exactly one read of the journaled event: the retried snapshot costs
    // nothing, the event is read once and is the hit, and the budget is then spent
    // before the later session.
    let (retried, reads) = search(size, bump_status);
    // Two attempts for the journaled session; the later session is never snapshotted.
    assert_eq!(attempts.get(), 2, "{retried}");
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = retried["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{retried}");
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(retried["scanned"]["sessions"], 1, "{retried}");
    assert_eq!(retried["scanned"]["events"], 1, "{retried}");
    assert!(
        retried["incomplete_reasons"].to_string().contains(&format!(
            "scan stopped: byte budget of {size} bytes exhausted"
        )),
        "{retried}"
    );

    // Budget to spare: the same retry, and both sessions are searched in full.
    let (twice, reads) = search(2 * size, bump_status);
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = twice["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{twice}");
    assert_eq!(twice["scanned"]["sessions"], 2, "{twice}");
    assert_eq!(twice["scanned"]["events"], 2, "{twice}");
    assert_eq!(twice["incomplete"], false, "{twice}");

    // A snapshot that fails leaves the budget untouched for the sessions after it.
    let (failed, reads) = search(size, drop_manifest);
    assert!(reads.is_empty(), "{reads:?}");
    let hits = failed["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{failed}");
    assert_eq!(hits[0]["session"], "session-zzz", "{failed}");
    assert_eq!(failed["scanned"]["sessions"], 1, "{failed}");
    assert_eq!(failed["incomplete"], true, "{failed}");
    let reasons = failed["incomplete_reasons"].to_string();
    assert!(reasons.contains("manifest"), "{reasons}");
    assert!(!reasons.contains("byte budget"), "{reasons}");
}

/// Points `link` at `target` as a directory symlink, or on Windows a junction when
/// symlinks need a privilege. Returns false where neither can be created.
fn link_directory(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link).is_ok()
        || Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|output| output.status.success());
    created && fs::symlink_metadata(link).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[test]
fn queries_reject_an_events_directory_link_before_reading_through_it() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-link");
    let message = format!("needle {}", "x".repeat(64));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    // The committed event now lives outside the state root, behind a link at `events`.
    let target = outside.path().join("events");
    fs::rename(directory.join("events"), &target).unwrap();
    if !link_directory(&target, &directory.join("events")) {
        eprintln!("skipped: this environment cannot create a directory link");
        return;
    }
    let external_event = target.join(&event_id);
    assert_eq!(
        read_json::<SessionEvent>(&external_event).unwrap().message,
        message
    );

    // The general snapshot rejects the link instead of reporting the event behind it.
    let error = query::request_result(&directory, &request_id).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    let search = cli_search(
        root.path(),
        &["search", "needle", "--all-workspaces", "--json"],
    );
    assert!(search["hits"].as_array().unwrap().is_empty(), "{search}");
    assert_eq!(search["scanned"]["events"], 0, "{search}");
    assert!(
        search["incomplete_reasons"]
            .to_string()
            .contains("events directory is a symlink"),
        "{search}"
    );

    // Nothing behind the link is opened: a record that could not even be read there
    // changes neither verdict.
    fs::remove_file(&external_event).unwrap();
    fs::create_dir(&external_event).unwrap();
    let error = query::request_result(&directory, &request_id).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    let search = cli_search(
        root.path(),
        &["search", "needle", "--all-workspaces", "--json"],
    );
    let reasons = search["incomplete_reasons"].to_string();
    assert!(
        reasons.contains("events directory is a symlink"),
        "{reasons}"
    );
    assert!(!reasons.contains("non-regular"), "{reasons}");
}

// ---------------------------------------------------------------------------
// Review round 5: per-event limits outside search, cached publication reads count,
// close settles without events/
// ---------------------------------------------------------------------------

/// A snapshot hook that changes the status record once, so the first snapshot fails its
/// consistency check and is retried exactly once.
fn bump_status_once() -> impl FnMut(&Path) {
    let mut fired = false;
    move |directory: &Path| {
        if !fired {
            fired = true;
            update_status(directory, "working", None, None).unwrap();
        }
    }
}

#[test]
fn ordinary_queries_keep_the_per_event_limit_across_a_forced_retry() {
    // A committed 40 MiB event is within the 64 MiB event limit. When the first snapshot
    // fails its consistency check and is retried, the second attempt must compare the
    // unchanged event within the same per-event limit: subtracting the first attempt's
    // read would leave 24 MiB and turn a valid published result into `recovery_required`.
    // A search's snapshot reads nothing, so its retry cannot cost the budget either.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-large");
    let message = format!("needle {}", "x".repeat(40 * 1024 * 1024));
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, &message, &message);
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let size = fs::metadata(&event_path).unwrap().len();
    assert!(
        size > EVENT_READ_LIMIT / 2 && size <= EVENT_READ_LIMIT,
        "{size}"
    );
    // Reading 40 MiB twice outlasts the production retry window in a debug build; the
    // retry itself, not its timing, is under test.
    let retried = |run: &dyn Fn() -> serde_json::Value| {
        query::with_snapshot_retry_window(Duration::from_secs(60), || {
            query::with_snapshot_hook(bump_status_once(), run)
        })
    };

    let result = retried(&|| query::request_result(&directory, &request_id).unwrap());
    assert_eq!(result["request_state"], "completed");
    assert_eq!(result["event_id"], event_id);
    assert_eq!(result["result"].as_str().map(str::len), Some(message.len()));

    let latest = retried(&|| {
        cli_result(
            root.path(),
            &["result", "session-large", "--latest", "--json"],
        )
    });
    assert_eq!(latest["request_state"], "completed");
    assert_eq!(latest["event_id"], event_id);
    assert_eq!(latest["request_id"], request_id);

    // The same forced retry inside a search: the retried snapshot reads nothing, and the
    // record is read once, at its scan position, within the 64 MiB search budget it fits.
    let (search, reads) = with_publication_read_log(|| {
        retried(&|| {
            cli_search(
                root.path(),
                &["search", "needle", "--all-workspaces", "--json"],
            )
        })
    });
    assert_eq!(reads, std::slice::from_ref(&event_path));
    let hits = search["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{}", search["incomplete_reasons"]);
    assert_eq!(hits[0]["request_id"], request_id);
    assert_eq!(hits[0]["event_id"], event_id);
    assert_eq!(search["scanned"]["events"], 1);
    assert_eq!(
        search["incomplete"], false,
        "{}",
        search["incomplete_reasons"]
    );
}

#[test]
fn search_stops_at_the_event_budget_before_reading_a_journaled_event() {
    // 5,000 ordinary events use up the event budget exactly; the committed journaled
    // event that sorts after them is one more event than the budget allows, so the scan
    // must stop before it without ever opening it, exactly as it does once the journal
    // is gone.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-many");
    let (request_id, event_path) =
        seed_event_written_completion_with(&directory, "needle at the end", "needle at the end");
    let event_id = event_path.file_name().unwrap().to_str().unwrap().to_owned();
    let filler = serde_json::to_vec_pretty(&oversized_sample_event("filler")).unwrap();
    for index in 0..5_000 {
        let name = format!("event-0-{index:05}.json");
        assert!(
            name.as_str() < event_id.as_str(),
            "{name} sorts after {event_id}"
        );
        fs::write(directory.join("events").join(name), &filler).unwrap();
    }
    let search = || {
        cli_search(
            root.path(),
            &["search", "needle", "--all-workspaces", "--json"],
        )
    };

    let (journaled, reads) = with_publication_read_log(search);
    assert!(
        reads.is_empty(),
        "the journaled event was opened: {reads:?}"
    );
    assert!(
        journaled["hits"].as_array().unwrap().is_empty(),
        "{journaled}"
    );
    assert_eq!(journaled["scanned"]["events"], 5_000, "{journaled}");
    assert_eq!(journaled["incomplete"], true, "{journaled}");
    assert!(
        journaled["incomplete_reasons"]
            .to_string()
            .contains("scan stopped: event budget of 5000 reads exhausted"),
        "{journaled}"
    );

    // `sessions` recovers the completion and removes the journal; the same scan must
    // report the same events, the same cut-off, and the same reason.
    cli_sessions(root.path());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let published = search();
    assert_eq!(published["hits"], journaled["hits"]);
    assert_eq!(published["scanned"], journaled["scanned"]);
    assert_eq!(published["incomplete"], journaled["incomplete"]);
    assert_eq!(
        published["incomplete_reasons"],
        journaled["incomplete_reasons"]
    );
}

#[test]
fn close_settles_a_journaled_completion_when_the_events_directory_is_missing() {
    // A journal with no `events` directory has no event to verify: the close keeps its
    // tombstone, releases the claim, and discards the journal, and the session can then
    // be listed, closed again, and pruned. Before this fix the directory check failed the
    // close after the tombstone, leaving claim and journal installed forever.
    for journaled_event in [JournaledEventState::Committed, JournaledEventState::Absent] {
        let fixture = seed_close_fixture(journaled_event);
        let label = format!("{journaled_event:?}");
        fs::remove_dir_all(fixture.directory.join("events")).unwrap();
        let close = || {
            close_session_state_with_error(
                &fixture.directory,
                Some("closed by the maintainer".to_owned()),
                |_| Ok(terminal::CloseOutcome::Closed),
            )
        };
        let assert_settled = |phase: &str| {
            assert!(
                fixture.directory.join(CLOSED_STATUS_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_CLAIM_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join(TURN_COMPLETION_FILE).exists(),
                "{label} {phase}"
            );
            assert!(
                !fixture.directory.join("events").exists(),
                "{label} {phase}: the close created an events directory"
            );
            assert_eq!(
                request_state(&fixture.directory, &fixture.request_id),
                ("unresolved".to_owned(), "closed".to_owned()),
                "{label} {phase}"
            );
        };

        close().unwrap();
        assert_settled("after the close");
        let tombstone_before = fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap();
        let listing = cli_sessions(fixture.root.path());
        assert_eq!(listing.len(), 1, "{label}: {listing:?}");
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        assert_settled("after sessions");
        close().unwrap();
        assert_settled("after the second close");
        assert_eq!(
            fs::read(fixture.directory.join(CLOSED_STATUS_FILE)).unwrap(),
            tombstone_before,
            "{label}: the tombstone changed"
        );
        assert_eq!(
            prune_closed_sessions(fixture.root.path(), u128::MAX).unwrap(),
            ["session-fault"],
            "{label}"
        );
        assert!(!fixture.directory.exists(), "{label}");
    }

    // An interrupted close (tombstone written, nothing else) converges the same way.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    update_status(
        &fixture.directory,
        "closed",
        None,
        Some("closed by the maintainer".to_owned()),
    )
    .unwrap();
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();
    assert!(recover_pending_completion(&fixture.directory).unwrap());
    assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&fixture.directory).unwrap());
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
}

#[test]
fn close_still_rejects_a_linked_events_directory() {
    // Settling a missing directory must not weaken link rejection: a link at `events`
    // fails the close, nothing behind the link is opened or moved, and the claim and
    // journal stay installed as the evidence of the unsettled completion.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("events");
    fs::rename(fixture.directory.join("events"), &target).unwrap();
    if !link_directory(&target, &fixture.directory.join("events")) {
        eprintln!("skipped: this environment cannot create a directory link");
        return;
    }
    let error = close_session_state_with_error(
        &fixture.directory,
        Some("closed by the maintainer".to_owned()),
        |_| Ok(terminal::CloseOutcome::Closed),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(target.join(&fixture.pending.event_file).exists());
    assert!(
        !target
            .join(format!(
                "{UNPUBLISHED_EVENT_PREFIX}{}",
                fixture.pending.event_file
            ))
            .exists()
    );
    // The interrupted-close convergence refuses the link the same way.
    let error = recover_pending_completion(&fixture.directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is a symlink"),
        "{error:#}"
    );
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
}

#[test]
fn claim_free_recovery_refuses_an_equivalent_event_in_another_encoding() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-compact");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let mut pending = sample_completion(&claim.token, "late result");
    pending.event_file = claim.receipt.event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    // Journal, event, terminal status, and claim release all exist, but the event bytes
    // are compact JSON: the same value, not the journal's canonical write.
    let event_path = directory.join("events").join(&pending.event_file);
    fs::write(&event_path, serde_json::to_vec(&pending.event).unwrap()).unwrap();
    update_status(&directory, "ready", None, None).unwrap();
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    let stored: SessionEvent = read_json(&event_path).unwrap();
    assert_eq!(stored, pending.event);

    let before = request_state(&directory, &request_id);
    assert_eq!(before, ("recovery_required".to_owned(), "ready".to_owned()));
    let error = recover_pending_completion(&directory).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("claim-free pending completion event does not match its journal"),
        "{error:#}"
    );
    let listing = cli_sessions(root.path());
    assert_eq!(listing[0]["state"], "ready");
    assert_eq!(request_state(&directory, &request_id), before);
    assert!(directory.join(TURN_COMPLETION_FILE).exists());
    assert!(event_path.exists());

    // Close settles the record the way it settles every event the journal disagrees
    // with: set aside under a name no query reads, never published.
    close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    assert_eq!(
        request_state(&directory, &request_id),
        ("unresolved".to_owned(), "closed".to_owned())
    );
    assert!(!event_path.exists());
    let quarantined = directory
        .join("events")
        .join(format!("{UNPUBLISHED_EVENT_PREFIX}{}", pending.event_file));
    let kept: SessionEvent = read_json(&quarantined).unwrap();
    assert_eq!(kept, pending.event);
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!recover_pending_completion(&directory).unwrap());
}

// ---------------------------------------------------------------------------
// Review round 6: journaled events read at scan position, quarantine durable before
// its evidence is discarded, dead owners repaired before completion recovery
// ---------------------------------------------------------------------------

#[test]
fn search_reads_a_journaled_event_only_at_its_scan_position() {
    // An older published event and a later committed journaled event, with a budget that
    // fits either record but not both. The scan spends its budget in filename order: the
    // older result is read and found, and the journaled event is the record that does
    // not fit. Reading the journaled event ahead of the scan reported no hit while the
    // journal existed and the older hit as soon as `sessions` removed the journal.
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-order");
    let later = format!("needle {}", "x".repeat(4000));
    let (request_id, later_path) = seed_event_written_completion_with(&directory, &later, &later);
    let later_id = later_path.file_name().unwrap().to_str().unwrap().to_owned();
    let older_id = "event-0-older.json";
    assert!(
        older_id < later_id.as_str(),
        "{older_id} sorts after {later_id}"
    );
    let mut older = read_json::<SessionEvent>(&later_path).unwrap();
    older.message = format!("needle {}", "y".repeat(3000));
    let older_path = directory.join("events").join(older_id);
    write_json_atomic(&older_path, &older).unwrap();
    let older_size = fs::metadata(&older_path).unwrap().len();
    let later_size = fs::metadata(&later_path).unwrap().len();
    let budget = (older_size + later_size - 1).to_string();
    let search = || {
        cli_search(
            root.path(),
            &[
                "search",
                "needle",
                "--all-workspaces",
                "--max-bytes",
                &budget,
                "--json",
            ],
        )
    };

    let (journaled, reads) = with_publication_read_log(search);
    let hits = journaled["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{journaled}");
    assert_eq!(hits[0]["event_id"], older_id, "{journaled}");
    assert_eq!(hits[0]["request_id"], serde_json::Value::Null);
    assert_eq!(journaled["scanned"]["events"], 1, "{journaled}");
    assert_eq!(journaled["incomplete"], true, "{journaled}");
    let reasons = journaled["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "byte budget of {budget} bytes exhausted; session-order/{later_id} is {later_size} bytes with {} bytes remaining",
            later_size - 1
        )),
        "{reasons}"
    );
    // The journaled event was never opened: at its position its size alone exceeds what
    // the budget has left, so the scan stopped without reading it.
    assert!(reads.is_empty(), "{reads:?}");

    // With budget for both records, both are hits, and the journaled event is opened
    // exactly once, for the publication comparison that is also its search read.
    let both = (older_size + later_size).to_string();
    let search_both = || {
        cli_search(
            root.path(),
            &[
                "search",
                "needle",
                "--all-workspaces",
                "--max-bytes",
                &both,
                "--json",
            ],
        )
    };
    let (complete, reads) = with_publication_read_log(search_both);
    assert_eq!(reads, std::slice::from_ref(&later_path));
    let hits = complete["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{complete}");
    assert_eq!(complete["incomplete"], false, "{complete}");
    assert_eq!(complete["scanned"]["events"], 2, "{complete}");
    assert!(
        hits.iter()
            .any(|hit| hit["event_id"] == later_id && hit["request_id"] == request_id),
        "{complete}"
    );

    // `sessions` recovers the completion and removes the journal; the committed event is
    // now an ordinary published record and both scans must report the same results.
    cli_sessions(root.path());
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(request_state(&directory, &request_id).0, "completed");
    let (published, reads) = with_publication_read_log(search);
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(published["hits"], journaled["hits"]);
    assert_eq!(published["scanned"], journaled["scanned"]);
    assert_eq!(published["incomplete"], journaled["incomplete"]);
    assert_eq!(
        published["incomplete_reasons"],
        journaled["incomplete_reasons"]
    );
    let (published, reads) = with_publication_read_log(search_both);
    assert!(reads.is_empty(), "{reads:?}");
    assert_eq!(published["hits"], complete["hits"]);
    assert_eq!(published["scanned"], complete["scanned"]);
    assert_eq!(published["incomplete"], complete["incomplete"]);
}

#[test]
fn recovery_syncs_an_interrupted_quarantine_before_discarding_its_journal() {
    // A close that stops right after moving a mismatched event aside, before `events/` is
    // synced, leaves the rename unsynced while the journal still proves the event was
    // unverified. The recovery that finishes the close must sync `events/` before it
    // removes the claim and the journal, or a later crash could bring the event back
    // without the journal that keeps it unpublished.
    let mut reproduced = false;
    for budget in 0.. {
        let fixture = seed_close_fixture(JournaledEventState::Mismatched);
        let events = fixture.directory.join("events");
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &fixture.directory,
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed),
                )
            })
        });
        let error = match outcome {
            Ok(()) => break,
            Err(error) => error,
        };
        assert!(injected_fault(&error), "{error:#}");
        let quarantined = events.join(format!(
            "{UNPUBLISHED_EVENT_PREFIX}{}",
            fixture.pending.event_file
        ));
        // The boundary under test: the event was moved aside, and `events/` was not
        // synced afterwards. (The close reports the later cleanup steps' faults, so the
        // boundary is recognised by its state, not by the error text.)
        if !quarantined.exists() || close_log.contains(&SyncRecord::Directory(events.clone())) {
            continue;
        }
        reproduced = true;
        assert!(!fixture.event_path.exists());
        assert!(fixture.directory.join(CLOSED_STATUS_FILE).exists());
        assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());

        let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(changed.unwrap());
        let events_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(events.clone()));
        let session_synced = log
            .iter()
            .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
        assert!(
            matches!(
                (events_synced, session_synced),
                (Some(events), Some(session)) if events < session
            ),
            "the quarantine rename was not made durable before the session records changed: {log:?}"
        );
        assert!(quarantined.exists());
        assert!(!fixture.event_path.exists());
        assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
        assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned())
        );
        // Converged: a second recovery changes nothing and syncs nothing.
        let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
        assert!(!again.unwrap());
        assert!(log.is_empty(), "{log:?}");
        break;
    }
    assert!(
        reproduced,
        "no close boundary stopped between the quarantine rename and its sync"
    );
}

/// [`seed_close_fixture`] with a committed event, in `state`, owned by a process that has
/// already exited, and then stripped of its `events/` directory: a journal that completion
/// recovery can no longer settle on a session only repair can close.
fn seed_dead_owner_with_unrecoverable_journal(state: &str) -> CloseFixture {
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    update_status(&fixture.directory, state, None, None).unwrap();
    write_dead_owner(&fixture.directory);
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();
    fixture
}

fn write_dead_owner(directory: &Path) {
    let pid = reaped_child_pid();
    write_json_atomic(
        &directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            managed_session_id: Some("session-close123".to_owned()),
            windows_process_identity: test_windows_process_identity(pid).or_else(|| {
                cfg!(windows).then(|| terminal::WindowsProcessIdentity {
                    creation_time: 0,
                    executable_path: String::new(),
                })
            }),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
}

#[test]
fn sessions_repairs_a_dead_owner_whose_journal_cannot_be_recovered() {
    for state in ["working", "exited"] {
        let fixture = seed_dead_owner_with_unrecoverable_journal(state);
        let directory = fixture.directory.clone();
        let label = format!("state {state}");
        // The missing directory is damage that completion recovery refuses.
        let error = recover_pending_completion(&directory).unwrap_err();
        assert!(
            format!("{error:#}").contains("events directory is missing"),
            "{label}: {error:#}"
        );

        // Repair does not stop at that damage: the owner is dead, so the session is
        // closed, which settles the journal the way every close settles a journal with
        // no event to verify, and the damage is reported in the close error.
        if state == "working" && cfg!(windows) {
            assert!(
                repair_dead_native_owner_with_terminal_close(&directory, |session| {
                    assert_eq!(session.kind, terminal::TerminalKind::WindowsConsole);
                    Ok(terminal::CloseOutcome::Missing)
                })
                .unwrap(),
                "{label}"
            );
        }
        let listing = cli_sessions(fixture.root.path());
        assert_eq!(listing.len(), 1, "{label}: {listing:?}");
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "closed", "{label}");
        let close_error = status.error.clone().unwrap_or_default();
        assert!(
            close_error.contains("is no longer running")
                && close_error.contains("events directory is missing"),
            "{label}: {close_error}"
        );
        assert!(directory.join(CLOSED_STATUS_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join(TURN_COMPLETION_FILE).exists(), "{label}");
        assert!(!directory.join(TERMINAL_HANDLE_FILE).exists(), "{label}");
        assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists(), "{label}");
        assert!(
            !directory.join("events").exists(),
            "{label}: the repair created an events directory"
        );
        assert_eq!(
            request_state(&directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned()),
            "{label}"
        );

        // Explicit close of the repaired session is idempotent: no adapter call, no
        // replacement claim, no event recreated, the tombstone unchanged.
        let tombstone = fs::read(directory.join(CLOSED_STATUS_FILE)).unwrap();
        let mut adapter_calls = 0;
        for _ in 0..2 {
            close_repaired_session_state(&directory, |_| {
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed)
            })
            .unwrap();
        }
        assert_eq!(adapter_calls, 0, "{label}");
        assert_eq!(
            fs::read(directory.join(CLOSED_STATUS_FILE)).unwrap(),
            tombstone,
            "{label}: the tombstone changed"
        );
        assert!(!directory.join(TURN_CLAIM_FILE).exists(), "{label}");
        assert!(!directory.join("events").exists(), "{label}");
        assert!(!recover_pending_completion(&directory).unwrap(), "{label}");
        assert!(!repair_dead_native_owner(&directory).unwrap(), "{label}");
        assert_eq!(
            request_state(&directory, &fixture.request_id),
            ("unresolved".to_owned(), "closed".to_owned()),
            "{label}"
        );
    }
}

#[test]
fn a_live_owner_keeps_unrecoverable_journal_damage_as_a_repair_error() {
    // The same damage under a live owner is not repaired away: the error is reported and
    // the claim, the journal, and the status stay exactly as they were.
    let fixture = seed_close_fixture(JournaledEventState::Committed);
    let pid = std::process::id();
    write_json_atomic(
        &fixture.directory.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid,
            managed_session_id: Some("session-close123".to_owned()),
            windows_process_identity: test_windows_process_identity(pid),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    fs::remove_dir_all(fixture.directory.join("events")).unwrap();

    let error = repair_dead_native_owner(&fixture.directory).unwrap_err();
    assert!(
        format!("{error:#}").contains("events directory is missing"),
        "{error:#}"
    );
    let listing = cli_sessions(fixture.root.path());
    assert_eq!(listing[0]["state"], "working", "{listing:?}");
    assert!(fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert!(!fixture.directory.join(CLOSED_STATUS_FILE).exists());
    assert!(fixture.directory.join(TERMINAL_HANDLE_FILE).exists());
}

// ---------------------------------------------------------------------------
// Review round 7: a committed event's directory entry is made durable before the
// journal that proves it is discarded
// ---------------------------------------------------------------------------

/// A completion stopped at the write boundary between the event's rename and the sync of
/// its directory entry: the event stands at its journal's path and matches it byte for
/// byte, the claim and the journal are still installed, and `events/` has not been synced
/// since the rename. Without a barrier, both recovery and close would accept the event as
/// committed and durably remove the journal, and a later crash could then lose the
/// event's directory entry together with the only evidence that it was the result.
struct UnsyncedCommittedEvent {
    _root: tempfile::TempDir,
    directory: PathBuf,
    request_id: String,
    event_path: PathBuf,
}

fn seed_completion_stopped_before_its_events_sync() -> UnsyncedCommittedEvent {
    for budget in 0.. {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fault");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, "working", None, None).unwrap();
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token.clone();
        let request_id = claim.receipt.request_id.clone();
        let event_path = directory.join("events").join(&claim.receipt.event_file);
        claim.retain();
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                record_provider_result_for_claim(
                    &directory,
                    FirstPartyCli::Codex,
                    "committed result",
                    Some("provider-session".to_owned()),
                    Some("provider-turn".to_owned()),
                    Some(&claim_token),
                )
            })
        });
        let error =
            outcome.expect_err("the completion finished before it stopped between rename and sync");
        assert!(injected_fault(&error), "{error:#}");
        // The boundary is recognised by its state: the event exists at its final path and
        // no sync of `events/` has happened. Every earlier budget stops before the event's
        // rename; every later one has synced the directory.
        if !event_path.exists() || log.contains(&SyncRecord::Directory(directory.join("events"))) {
            continue;
        }
        assert!(directory.join(TURN_CLAIM_FILE).exists());
        assert!(directory.join(TURN_COMPLETION_FILE).exists());
        return UnsyncedCommittedEvent {
            _root: root,
            directory,
            request_id,
            event_path,
        };
    }
    unreachable!("the fault budget sweep never ends without returning or panicking")
}

const COMMITTED_EVENT_SYNC_LABEL: &str = "syncing a committed completion event's directory";

#[test]
fn recovery_syncs_a_committed_event_before_discarding_its_journal() {
    // An uninterrupted recovery syncs `events/` before it touches any session record: the
    // status rewrite, the claim release, and the journal removal all sync the session
    // directory, and the events sync must come first.
    let fixture = seed_completion_stopped_before_its_events_sync();
    let events = fixture.directory.join("events");
    let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(changed.unwrap());
    let events_synced = log
        .iter()
        .position(|record| record == &SyncRecord::Directory(events.clone()));
    let session_synced = log
        .iter()
        .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
    assert!(
        matches!(
            (events_synced, session_synced),
            (Some(events), Some(session)) if events < session
        ),
        "the committed event was not made durable before the session records changed: {log:?}"
    );
    assert!(fixture.event_path.exists());
    assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    let status: SessionStatus = read_json(&fixture.directory.join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    // Converged: a repeated recovery changes nothing and syncs nothing.
    let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(!again.unwrap());
    assert!(log.is_empty(), "{log:?}");

    // Every interruption of that recovery: the claim and the journal outlive the event's
    // unsynced entry. Whenever either is gone, `events/` was synced first, and the very
    // first boundary the recovery can stop at is the sync itself.
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_completion_stopped_before_its_events_sync();
        let events = fixture.directory.join("events");
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || recover_pending_completion(&fixture.directory))
        });
        let claim_kept = fixture.directory.join(TURN_CLAIM_FILE).exists();
        let journal_kept = fixture.directory.join(TURN_COMPLETION_FILE).exists();
        if !(claim_kept && journal_kept) {
            assert!(
                log.contains(&SyncRecord::Directory(events.clone())),
                "budget {budget}: recovery discarded the journal's evidence before syncing events/: {log:?}"
            );
        }
        match outcome {
            Ok(changed) => {
                assert!(changed);
                assert!(!claim_kept && !journal_kept);
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
                assert!(fixture.event_path.exists());
                // The next holder converges (a stop after the journal's removal leaves it
                // nothing to change).
                recover_pending_completion(&fixture.directory).unwrap();
                assert!(!fixture.directory.join(TURN_CLAIM_FILE).exists());
                assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "ready".to_owned())
                );
            }
        }
    }
    // Recovery reports its own first failure, so the label is exact here.
    assert!(
        labels
            .first()
            .is_some_and(|label| label.contains(COMMITTED_EVENT_SYNC_LABEL)),
        "the first recovery boundary is not the committed event's directory sync: {labels:?}"
    );
}

#[test]
fn close_syncs_a_committed_event_before_discarding_its_journal() {
    // A close of the same interrupted session writes its tombstone first, so the events
    // sync cannot precede every session-directory sync; the property is that no
    // interruption of the close, nor of the recovery that finishes an interrupted close,
    // removes the claim or the journal before `events/` was synced. The finished close
    // leaves the event published and nothing for a later recovery to sync. The close
    // reports the later cleanup steps' faults rather than the barrier's own, so the
    // boundary is recognised by its state: tombstone written, claim and journal kept,
    // `events/` not yet synced.
    let mut stopped_before_barrier = false;
    for budget in 0.. {
        let fixture = seed_completion_stopped_before_its_events_sync();
        let events = fixture.directory.join("events");
        let claim_path = fixture.directory.join(TURN_CLAIM_FILE);
        let completion_path = fixture.directory.join(TURN_COMPLETION_FILE);
        let (outcome, close_log) = with_sync_log(|| {
            with_fault_budget(budget, || {
                close_session_state_with_error(
                    &fixture.directory,
                    Some("closed by the maintainer".to_owned()),
                    |_| Ok(terminal::CloseOutcome::Closed),
                )
            })
        });
        let events_synced = close_log.contains(&SyncRecord::Directory(events.clone()));
        if !(claim_path.exists() && completion_path.exists()) {
            assert!(
                events_synced,
                "budget {budget}: close discarded the journal's evidence before syncing events/: {close_log:?}"
            );
        }
        match outcome {
            Ok(()) => {
                assert!(events_synced);
                assert!(!claim_path.exists());
                assert!(!completion_path.exists());
                assert!(fixture.event_path.exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "closed".to_owned())
                );
                let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
                assert!(!again.unwrap());
                assert!(log.is_empty(), "{log:?}");
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
            }
        }
        // Between the status rewrite and the barrier: the recovery that finishes this
        // close has no session record to rewrite before it reaches the barrier.
        let at_barrier = fixture.directory.join(CLOSED_STATUS_FILE).exists()
            && read_json::<SessionStatus>(&fixture.directory.join("status.json"))
                .is_ok_and(|status| status.state == "closed")
            && claim_path.exists()
            && completion_path.exists()
            && !events_synced;
        stopped_before_barrier |= at_barrier;
        // The next lifecycle-lock holder finishes the interrupted close (or, before the
        // tombstone, publishes the completion); either way it must not discard the
        // journal without the barrier having run in one of the two passes.
        let (recovered, recovery_log) =
            with_sync_log(|| recover_pending_completion(&fixture.directory));
        recovered.unwrap();
        assert!(!completion_path.exists());
        assert!(!claim_path.exists());
        assert!(
            events_synced || recovery_log.contains(&SyncRecord::Directory(events.clone())),
            "budget {budget}: neither the close nor its recovery synced events/: {close_log:?} {recovery_log:?}"
        );
        if at_barrier {
            let events_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(events.clone()));
            let session_synced = recovery_log
                .iter()
                .position(|record| record == &SyncRecord::Directory(fixture.directory.clone()));
            assert!(
                matches!(
                    (events_synced, session_synced),
                    (Some(events), Some(session)) if events < session
                ),
                "budget {budget}: the interrupted close's recovery changed session records before syncing events/: {recovery_log:?}"
            );
        }
        assert!(fixture.event_path.exists());
        // Before the tombstone the close never committed, so recovery published the
        // completion on a session that stays ready; after it, the close is finished.
        let session_state = if fixture.directory.join(CLOSED_STATUS_FILE).exists() {
            "closed"
        } else {
            "ready"
        };
        assert_eq!(
            request_state(&fixture.directory, &fixture.request_id),
            ("completed".to_owned(), session_state.to_owned()),
            "budget {budget}"
        );
    }
    assert!(
        stopped_before_barrier,
        "no close boundary stopped after the tombstone and before the committed event's directory sync"
    );
}

/// A completion whose event is committed, whose terminal status is written, and whose
/// claim is released, but whose `events/` entry was never synced: the run that released
/// the claim stopped before it removed the journal, and the sync between the event's
/// rename and the status write did not happen either (or is not trusted to have). Only
/// the journal still says the event is the result, so the claim-free recovery must sync
/// `events/` before it removes the journal, without any earlier sync helping it.
fn seed_claim_free_completion_with_an_unsynced_event() -> UnsyncedCommittedEvent {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-claimfree");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, "working", None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt.request_id.clone();
    let mut pending = sample_completion(&claim.token, "late result");
    pending.event_file = claim.receipt.event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    // The journal's exact bytes at the event path, written without a sync of `events/`.
    fs::write(
        &event_path,
        serde_json::to_vec_pretty(&pending.event).unwrap(),
    )
    .unwrap();
    update_status(&directory, "ready", None, None).unwrap();
    fs::remove_file(directory.join(TURN_CLAIM_FILE)).unwrap();
    assert_eq!(
        journaled_event_state(&directory, &pending).unwrap(),
        JournaledEventState::Committed
    );
    UnsyncedCommittedEvent {
        _root: root,
        directory,
        request_id,
        event_path,
    }
}

#[test]
fn claim_free_recovery_syncs_the_committed_event_before_discarding_its_journal() {
    // The claim-free path has no session record to rewrite and no claim to release, so
    // its whole sync log is the barrier followed by the journal removal's own sync.
    let fixture = seed_claim_free_completion_with_an_unsynced_event();
    let events = fixture.directory.join("events");
    // Read-only queries already report the byte-identical event as published; what the
    // recovery adds is the durability of its entry before the journal goes.
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    let (changed, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(changed.unwrap());
    assert_eq!(
        log,
        [
            SyncRecord::Directory(events.clone()),
            SyncRecord::Directory(fixture.directory.clone()),
        ],
        "{log:?}"
    );
    assert!(fixture.event_path.exists());
    assert!(!fixture.directory.join(TURN_COMPLETION_FILE).exists());
    assert_eq!(
        request_state(&fixture.directory, &fixture.request_id),
        ("completed".to_owned(), "ready".to_owned())
    );
    let (again, log) = with_sync_log(|| recover_pending_completion(&fixture.directory));
    assert!(!again.unwrap());
    assert!(log.is_empty(), "{log:?}");

    // Every interruption: the journal never goes before `events/` was synced, the first
    // boundary the path can stop at is the barrier itself, and the next holder converges
    // through the same barrier whenever the journal is still there.
    let mut labels = Vec::new();
    for budget in 0.. {
        let fixture = seed_claim_free_completion_with_an_unsynced_event();
        let events = fixture.directory.join("events");
        let completion_path = fixture.directory.join(TURN_COMPLETION_FILE);
        let (outcome, log) = with_sync_log(|| {
            with_fault_budget(budget, || recover_pending_completion(&fixture.directory))
        });
        let journal_kept = completion_path.exists();
        if !journal_kept {
            assert!(
                log.contains(&SyncRecord::Directory(events.clone())),
                "budget {budget}: the journal was discarded before events/ was synced: {log:?}"
            );
        }
        match outcome {
            Ok(changed) => {
                assert!(changed);
                assert!(!journal_kept);
                break;
            }
            Err(error) => {
                assert!(injected_fault(&error), "{error:#}");
                labels.push(format!("{error:#}"));
                assert!(fixture.event_path.exists());
                let (recovered, log) =
                    with_sync_log(|| recover_pending_completion(&fixture.directory));
                let recovered = recovered.unwrap();
                assert_eq!(recovered, journal_kept, "budget {budget}");
                if journal_kept {
                    assert_eq!(
                        log[0],
                        SyncRecord::Directory(events.clone()),
                        "budget {budget}: {log:?}"
                    );
                } else {
                    assert!(log.is_empty(), "budget {budget}: {log:?}");
                }
                assert!(!completion_path.exists());
                assert!(fixture.event_path.exists());
                assert_eq!(
                    request_state(&fixture.directory, &fixture.request_id),
                    ("completed".to_owned(), "ready".to_owned())
                );
            }
        }
    }
    assert!(
        labels
            .first()
            .is_some_and(|label| label.contains(COMMITTED_EVENT_SYNC_LABEL)),
        "the first claim-free boundary is not the committed event's directory sync: {labels:?}"
    );
    // Barrier, journal removal, and the removal's sync: no other boundary exists.
    assert_eq!(labels.len(), 3, "{labels:?}");
}

// ---------------------------------------------------------------------------------------
// `reopen`: continuing a closed session's provider conversation in a new session.
// ---------------------------------------------------------------------------------------

const REOPEN_TEST_CONVERSATION: &str = "6928ca1c-1234-4abc-8def-0123456789ab";

fn write_reopen_test_manifest(
    directory: &Path,
    id: &str,
    provider: &str,
    provider_path: PathBuf,
    workspace: PathBuf,
    with_policy: bool,
) {
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: id.to_owned(),
            provider: provider.to_owned(),
            provider_path,
            provider_version: "2.1.281".to_owned(),
            workspace,
            title: id.to_owned(),
            model: with_policy.then(|| "Fable5".to_owned()),
            effort: with_policy.then(|| "max".to_owned()),
            yolo: with_policy,
            created_unix_ms: 1,
        },
    )
    .unwrap();
}

fn reopen_test_terminal(id: &str) -> terminal::TerminalSession {
    terminal::TerminalSession {
        kind: terminal::TerminalKind::Iterm2,
        id: format!("{id}-terminal"),
        tab_id: None,
        window_id: None,
        managed_session_id: Some(id.to_owned()),
        windows_process_identity: None,
    }
}

// A closed session with one resolved request: its turn completed (or failed, when
// `completed` is false) with the given provider session id, its terminal handle was
// consumed into the tombstone, and its status is `closed` with `closed.json`. The source
// manifest deliberately carries yolo, model, and effort so inheritance would be visible.
fn write_closed_reopen_source(
    root: &Path,
    id: &str,
    provider: &str,
    provider_session_id: Option<&str>,
    completed: bool,
) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    write_reopen_test_manifest(
        &directory,
        id,
        provider,
        PathBuf::from("/opt/provider"),
        root.to_owned(),
        true,
    );
    write_json_atomic(
        &directory.join(TERMINAL_HANDLE_FILE),
        &reopen_test_terminal(id),
    )
    .unwrap();
    update_status(&directory, "running", None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let token = claim.token.clone();
    claim.retain();
    let provider = FirstPartyCli::from_str(provider).unwrap();
    if completed {
        record_provider_result_for_claim(
            &directory,
            provider,
            "done",
            provider_session_id.map(str::to_owned),
            Some("turn-1".to_owned()),
            Some(&token),
        )
        .unwrap();
    } else {
        record_provider_failure_for_claim(
            &directory,
            provider,
            "the only turn failed",
            provider_session_id.map(str::to_owned),
            None,
            Some(&token),
        )
        .unwrap();
    }
    close_session_state(&directory, |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    assert!(directory.join(CLOSED_STATUS_FILE).is_file());
    assert!(directory.join(TERMINAL_TOMBSTONE_FILE).is_file());
    directory
}

fn snapshot_directory(directory: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, current: &Path, files: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(name, fs::read(&path).unwrap());
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(directory, directory, &mut files);
    files
}

#[test]
fn reopen_parses_only_explicit_policy_and_rejects_ask_only_options() {
    let NativeCommand::Reopen(request) =
        parse_args(["reopen", "session-src1", "--prompt", "continue"]).unwrap()
    else {
        panic!("expected reopen");
    };
    assert_eq!(request.id, "session-src1");
    assert_eq!(request.prompt, "continue");
    assert!(!request.yolo);
    assert_eq!(request.model, None);
    assert_eq!(request.effort, None);
    assert_eq!(request.title, None);
    assert_eq!(request.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    assert!(!request.detach && !request.json);

    let NativeCommand::Reopen(request) = parse_args([
        "reopen",
        "session-src1",
        "--prompt",
        "continue",
        "--yolo",
        "--model",
        "Fable5",
        "--effort",
        "max",
        "--title",
        "again",
        "--timeout-secs",
        "30",
        "--detach",
        "--json",
    ])
    .unwrap() else {
        panic!("expected reopen");
    };
    assert!(request.yolo);
    assert_eq!(request.model.as_deref(), Some("Fable5"));
    assert_eq!(request.effort.as_deref(), Some("max"));
    assert_eq!(request.title.as_deref(), Some("again"));
    assert_eq!(request.timeout, Duration::from_secs(30));
    assert!(request.detach && request.json);

    for arguments in [
        vec!["reopen"],
        vec!["reopen", "session-src1"],
        vec!["reopen", "session-src1", "--prompt", " "],
        vec!["reopen", "not-a-session", "--prompt", "x"],
        vec![
            "reopen",
            "session-src1",
            "--prompt",
            "x",
            "--workspace",
            ".",
        ],
        vec![
            "reopen",
            "session-src1",
            "--prompt",
            "x",
            "--context-result",
            "session-a/request-1",
        ],
        vec![
            "reopen",
            "session-src1",
            "--prompt",
            "x",
            "--yolo",
            "--yolo",
        ],
    ] {
        assert!(parse_args(arguments.clone()).is_err(), "{arguments:?}");
    }
}

#[test]
fn concurrent_reopen_of_one_closed_session_admits_exactly_one_winner_and_leaves_the_source_unchanged()
 {
    let root = tempfile::tempdir().unwrap();
    let id = "session-reopensrc1";
    let source = write_closed_reopen_source(
        root.path(),
        id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let before = snapshot_directory(&source);

    let attempts = (0..8)
        .map(|_| {
            let source = source.clone();
            thread::spawn(move || claim_reopen_marker(&source, id, REOPEN_TEST_CONVERSATION))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|attempt| attempt.join().unwrap())
        .collect::<Vec<_>>();
    let (winners, losers): (Vec<_>, Vec<_>) = attempts.into_iter().partition(Result::is_ok);
    assert_eq!(winners.len(), 1);
    assert_eq!(losers.len(), 7);
    for loser in &losers {
        let error = loser.as_ref().unwrap_err();
        assert_eq!(
            reopen_refusal_gate(error),
            Some("already-reopened"),
            "{error:#}"
        );
    }
    let winner = winners.into_iter().next().unwrap().unwrap();
    winner.finalize("session-reopennew1").unwrap();

    let mut after = snapshot_directory(&source);
    let marker_text = after
        .remove(REOPEN_MARKER_FILE)
        .expect("the winner leaves its marker in the source");
    assert_eq!(after, before);
    let marker: ReopenMarker = serde_json::from_slice(&marker_text).unwrap();
    assert_eq!(marker.schema, 1);
    assert_eq!(marker.reopened_by.as_deref(), Some("session-reopennew1"));
    assert_eq!(marker.provider_session_id, REOPEN_TEST_CONVERSATION);

    let refused = claim_reopen_marker(&source, id, REOPEN_TEST_CONVERSATION).unwrap_err();
    assert_eq!(reopen_refusal_gate(&refused), Some("already-reopened"));
    assert!(
        format!("{refused:#}").contains("already reopened as session-reopennew1"),
        "{refused:#}"
    );
    let refused = inspect_reopen_source(&source, id).unwrap_err();
    assert_eq!(reopen_refusal_gate(&refused), Some("already-reopened"));
}

#[test]
fn reopen_marker_is_released_when_the_new_session_is_never_created() {
    let root = tempfile::tempdir().unwrap();
    let id = "session-reopensrc2";
    let source = write_closed_reopen_source(
        root.path(),
        id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    {
        let _claim = claim_reopen_marker(&source, id, REOPEN_TEST_CONVERSATION).unwrap();
        assert!(source.join(REOPEN_MARKER_FILE).is_file());
        assert_eq!(
            reopen_refusal_gate(
                &claim_reopen_marker(&source, id, REOPEN_TEST_CONVERSATION).unwrap_err()
            ),
            Some("already-reopened")
        );
    }
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    let claim = claim_reopen_marker(&source, id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize("session-reopennew2").unwrap();
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
}

#[test]
fn reopen_gates_refuse_open_unconverged_identity_less_and_delivery_uncertain_sources() {
    let root = tempfile::tempdir().unwrap();

    let open = root.path().join("session-reopenopen");
    fs::create_dir(&open).unwrap();
    fs::create_dir(open.join("events")).unwrap();
    write_reopen_test_manifest(
        &open,
        "session-reopenopen",
        "claude",
        PathBuf::from("/opt/claude"),
        root.path().to_owned(),
        false,
    );
    update_status(&open, "ready", None, None).unwrap();
    let error = inspect_reopen_source(&open, "session-reopenopen").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("source-not-closed"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("is ready"));

    let lingering = write_closed_reopen_source(
        root.path(),
        "session-reopenclaim",
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    fs::write(lingering.join(TURN_CLAIM_FILE), "1-2-3\n").unwrap();
    let error = inspect_reopen_source(&lingering, "session-reopenclaim").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("source-not-converged"),
        "{error:#}"
    );

    let no_identity =
        write_closed_reopen_source(root.path(), "session-reopennoid", "claude", None, true);
    let error = inspect_reopen_source(&no_identity, "session-reopennoid").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("source-identity-missing"),
        "{error:#}"
    );

    let only_failed =
        write_closed_reopen_source(root.path(), "session-reopenfail", "claude", None, false);
    let error = inspect_reopen_source(&only_failed, "session-reopenfail").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("source-identity-missing"),
        "{error:#}"
    );

    let unresolved = write_closed_reopen_source(
        root.path(),
        "session-reopenunres",
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let receipt = requests::create(&unresolved, "9-9-9", &[]).unwrap();
    assert!(!unresolved.join("events").join(&receipt.event_file).exists());
    let error = inspect_reopen_source(&unresolved, "session-reopenunres").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("request-unresolved"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains(&receipt.request_id));

    let healthy = write_closed_reopen_source(
        root.path(),
        "session-reopenok",
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let source = inspect_reopen_source(&healthy, "session-reopenok").unwrap();
    assert_eq!(source.provider, FirstPartyCli::Claude);
    assert_eq!(source.provider_session_id, REOPEN_TEST_CONVERSATION);
    assert!(valid_event_file_name(&source.event_id));
    assert!(healthy.join("events").join(&source.event_id).is_file());
    assert_eq!(source.manifest.id, "session-reopenok");
    assert!(!healthy.join(REOPEN_MARKER_FILE).exists());
}

#[test]
fn non_claude_sources_are_refused_with_each_adapters_own_reason() {
    let root = tempfile::tempdir().unwrap();
    let codex = write_closed_reopen_source(
        root.path(),
        "session-reopencodex",
        "codex",
        Some("01a0d22a-e41e-7661-b666-229f7f1e6435"),
        true,
    );
    let source = inspect_reopen_source(&codex, "session-reopencodex").unwrap();
    assert_eq!(source.provider, FirstPartyCli::Codex);
    let error = provider::verify_reopen_available(source.provider, &source.provider_session_id)
        .unwrap_err();
    assert!(
        format!("{error:#}").starts_with("reopen unsupported: Codex"),
        "{error:#}"
    );

    for (provider, expected) in [
        (FirstPartyCli::Agy, "reopen unsupported: Agy"),
        (FirstPartyCli::Pi, "reopen unsupported: Pi"),
    ] {
        let error =
            provider::verify_reopen_available(provider, "5e58ec26-0000-4000-8000-000000000000")
                .unwrap_err();
        assert!(format!("{error:#}").starts_with(expected), "{error:#}");
    }
    for provider in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        let error = provider::prepare_resume(
            provider,
            provider::ResumeContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: root.path(),
                provider_session_id: REOPEN_TEST_CONVERSATION,
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("reopen unsupported: "),
            "{error:#}"
        );
        let error = provider::other_resumed_conversation_holders(
            provider,
            provider::ResumedSessionContext {
                directory: root.path(),
                provider_session_id: REOPEN_TEST_CONVERSATION,
                deadline: Instant::now() + Duration::from_secs(1),
                wait_for_registration: true,
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("reopen unsupported: "),
            "{error:#}"
        );
    }
}

#[test]
fn late_hook_into_a_closed_source_records_nothing() {
    let root = tempfile::tempdir().unwrap();
    let source = write_closed_reopen_source(
        root.path(),
        "session-reopenlate",
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let old_marker = "<!-- agent-bridge-claude-turn:claude-turn-1-1-1 -->";
    write_json_atomic(
        &source.join("claude-pending-turn.json"),
        &serde_json::json!({
            "schema": 1,
            "request_id": "claude-turn-1-1-1",
            "marker": old_marker,
        }),
    )
    .unwrap();
    let stop = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": REOPEN_TEST_CONVERSATION,
        "last_assistant_message": format!("late answer\n{old_marker}"),
    });
    let stop_failure = serde_json::json!({
        "hook_event_name": "StopFailure",
        "session_id": REOPEN_TEST_CONVERSATION,
        "error": "late failure",
    });

    let before = snapshot_directory(&source);
    provider::handle_hook(FirstPartyCli::Claude, &source, &stop).unwrap();
    provider::handle_hook(FirstPartyCli::Claude, &source, &stop_failure).unwrap();
    assert_eq!(snapshot_directory(&source), before);

    fs::remove_file(source.join("claude-pending-turn.json")).unwrap();
    let before = snapshot_directory(&source);
    provider::handle_hook(FirstPartyCli::Claude, &source, &stop).unwrap();
    provider::handle_hook(FirstPartyCli::Claude, &source, &stop_failure).unwrap();
    assert_eq!(snapshot_directory(&source), before);
    assert_eq!(event_paths(&source).unwrap().len(), 1);
    assert_eq!(
        read_json::<SessionStatus>(&source.join("status.json"))
            .unwrap()
            .state,
        "closed"
    );
}

#[test]
fn late_hook_carrying_the_previous_marker_into_the_new_directory_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), "working", None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let token = claim.token.clone();
    claim.retain();
    let new_request = "claude-turn-2-2-2";
    write_json_atomic(
        &directory.path().join("claude-pending-turn.json"),
        &serde_json::json!({
            "schema": 1,
            "request_id": new_request,
            "marker": format!("<!-- agent-bridge-claude-turn:{new_request} -->"),
        }),
    )
    .unwrap();

    provider::handle_hook(
        FirstPartyCli::Claude,
        directory.path(),
        &serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": REOPEN_TEST_CONVERSATION,
            "last_assistant_message":
                "answer to the closed source\n<!-- agent-bridge-claude-turn:claude-turn-1-1-1 -->",
        }),
    )
    .unwrap();
    assert!(event_paths(directory.path()).unwrap().is_empty());
    assert_eq!(
        current_turn_claim_token(directory.path())
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "working"
    );

    provider::handle_hook(
        FirstPartyCli::Claude,
        directory.path(),
        &serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": REOPEN_TEST_CONVERSATION,
            "last_assistant_message":
                format!("answer to the reopened request\n<!-- agent-bridge-claude-turn:{new_request} -->"),
        }),
    )
    .unwrap();
    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.message, "answer to the reopened request");
    assert_eq!(event.turn_id.as_deref(), Some(new_request));
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some(REOPEN_TEST_CONVERSATION)
    );
    assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
    assert_eq!(
        read_json::<SessionStatus>(&directory.path().join("status.json"))
            .unwrap()
            .state,
        "ready"
    );
}

#[test]
fn reopened_session_close_consumes_only_its_own_handle_and_leaves_the_source_tombstone() {
    let root = tempfile::tempdir().unwrap();
    let source_id = "session-reopensrc3";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let new_id = "session-reopennew3";
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    let new = root.path().join(new_id);
    fs::create_dir(&new).unwrap();
    fs::create_dir(new.join("events")).unwrap();
    write_reopen_test_manifest(
        &new,
        new_id,
        "claude",
        PathBuf::from("/opt/claude"),
        root.path().to_owned(),
        false,
    );
    record_resumed_from(
        &new,
        &read_manifest(&new).unwrap(),
        &ResumedFrom {
            session: source_id.to_owned(),
            provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
            event_id: "event-1-1.json".to_owned(),
        },
    )
    .unwrap();
    write_json_atomic(
        &new.join(TERMINAL_HANDLE_FILE),
        &reopen_test_terminal(new_id),
    )
    .unwrap();
    update_status(&new, "running", None, None).unwrap();
    let source_before = snapshot_directory(&source);

    let mut closed_terminals = Vec::new();
    close_session_state(&new, |session| {
        closed_terminals.push(session.id.clone());
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(closed_terminals, [format!("{new_id}-terminal")]);
    assert!(new.join(TERMINAL_TOMBSTONE_FILE).is_file());
    assert!(!new.join(TERMINAL_HANDLE_FILE).exists());
    assert_eq!(snapshot_directory(&source), source_before);

    update_status(&new, "exited", Some(1), None).unwrap();
    update_status(
        &new,
        "failed",
        None,
        Some("provider exited after close".to_owned()),
    )
    .unwrap();
    let status: SessionStatus = read_json(&new.join("status.json")).unwrap();
    assert_eq!(status.state, "closed");
    assert_eq!(status.exit_code, None);
    assert_eq!(status.error, None);

    let mut close_calls = 0;
    close_session_state(&new, |_| {
        close_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(close_calls, 0);
    assert_eq!(snapshot_directory(&source), source_before);

    let mut close_calls = 0;
    close_session_state(&source, |_| {
        close_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(close_calls, 0);
    assert_eq!(
        read_json::<SessionStatus>(&source.join("status.json"))
            .unwrap()
            .state,
        "closed"
    );
    let marker: ReopenMarker = read_json(&source.join(REOPEN_MARKER_FILE)).unwrap();
    assert_eq!(marker.reopened_by.as_deref(), Some(new_id));
    assert_eq!(read_resumed_from(&source).unwrap(), None);
}

#[test]
fn inspect_reports_resumed_from_for_reopened_sessions_and_schema_one_readers_still_parse() {
    let root = tempfile::tempdir().unwrap();
    let id = "session-reopeninsp";
    let directory = root.path().join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    write_reopen_test_manifest(
        &directory,
        id,
        "claude",
        PathBuf::from("/opt/claude"),
        root.path().to_owned(),
        false,
    );
    update_status(&directory, "ready", None, None).unwrap();
    assert_eq!(
        query::inspect_value(&directory, id).unwrap()["resumed_from"],
        serde_json::Value::Null
    );
    assert_eq!(read_resumed_from(&directory).unwrap(), None);

    let resumed_from = ResumedFrom {
        session: "session-reopensrc4".to_owned(),
        provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
        event_id: "event-1-1.json".to_owned(),
    };
    record_resumed_from(
        &directory,
        &read_manifest(&directory).unwrap(),
        &resumed_from,
    )
    .unwrap();
    let value = query::inspect_value(&directory, id).unwrap();
    assert_eq!(
        value["resumed_from"],
        serde_json::json!({
            "session": "session-reopensrc4",
            "provider_session_id": REOPEN_TEST_CONVERSATION,
            "event_id": "event-1-1.json",
        })
    );
    assert_eq!(value["configured"]["yolo"], false);
    assert_eq!(value["configured"]["model"], serde_json::Value::Null);
    assert_eq!(read_resumed_from(&directory).unwrap(), Some(resumed_from));
    let manifest = read_manifest(&directory).unwrap();
    assert_eq!(manifest.schema, SESSION_SCHEMA);
    assert_eq!(manifest.id, id);
    assert_eq!(manifest.model, None);
    assert!(!manifest.yolo);
    let raw: serde_json::Value = read_json(&directory.join("manifest.json")).unwrap();
    assert_eq!(raw["schema"], 1);
    assert_eq!(raw["resumed_from"]["session"], "session-reopensrc4");
}

// Proves only what the wrapper passes: the official resume arguments and no policy
// argument the request did not state. Which model the reopened process then runs on is
// Claude's own resume decision and is not observable from a stub.
#[cfg(windows)]
#[test]
fn resumed_session_launch_passes_the_official_resume_plan_and_no_policy_the_request_omitted() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    fs::create_dir(&registry).unwrap();
    provider::override_claude_session_registry_for_test(Some(registry));
    let provider = root.path().join("claude.cmd");
    write_private(
        &provider,
        b"@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo 2.1.281\r\n  exit /b 0\r\n)\r\necho %*> \"%AGENT_BRIDGE_NATIVE_SESSION_DIR%\\argv.txt\"\r\n",
    )
    .unwrap();
    let id = "session-reopenlaunch";
    let directory = root.path().join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    write_reopen_test_manifest(
        &directory,
        id,
        "claude",
        provider.clone(),
        workspace.clone(),
        false,
    );
    record_resumed_from(
        &directory,
        &read_manifest(&directory).unwrap(),
        &ResumedFrom {
            session: "session-reopensrc9".to_owned(),
            provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
            event_id: "event-1-1.json".to_owned(),
        },
    )
    .unwrap();
    write_private(
        &directory.join("initial-prompt.txt"),
        native_delegation_prompt("parent", "review this again").as_bytes(),
    )
    .unwrap();
    update_status(&directory, "launching", None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();

    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert!(arguments.contains("--resume"), "{arguments}");
    assert!(arguments.contains(REOPEN_TEST_CONVERSATION), "{arguments}");
    assert!(arguments.contains("--name"), "{arguments}");
    assert!(arguments.contains(id), "{arguments}");
    assert!(arguments.contains("--settings"), "{arguments}");
    for inherited in [
        "--dangerously-skip-permissions",
        "--model",
        "--effort",
        "Fable",
        "review this again",
    ] {
        assert!(
            !arguments.contains(inherited),
            "{inherited} leaked into {arguments}"
        );
    }
    let settings: serde_json::Value = read_json(&directory.join("claude-settings.json")).unwrap();
    assert_eq!(settings["crossSessionInbound"], "accept");
    assert!(directory.join("initial-prompt.txt").is_file());
}

// A receipt whose recorded result exists but cannot be read, or belongs to another provider,
// resolves nothing, however healthy the latest identity-bearing event is.
#[test]
fn reopen_refuses_receipts_whose_recorded_result_is_empty_malformed_or_foreign() {
    let root = tempfile::tempdir().unwrap();
    let id = "session-reopenrcpt";
    let source = write_closed_reopen_source(
        root.path(),
        id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    assert!(inspect_reopen_source(&source, id).is_ok());
    let older_event = "event-1-1.json";
    let older_path = source.join("events").join(older_event);
    fs::create_dir_all(source.join("requests")).unwrap();
    write_json_atomic(
        &source.join("requests").join("1-1-1.json"),
        &requests::Receipt {
            schema: 1,
            request_id: "request-older".to_owned(),
            claim_token: "1-1-1".to_owned(),
            event_file: older_event.to_owned(),
            created_unix_ms: 1,
            source: None,
            context_sources: Vec::new(),
        },
    )
    .unwrap();
    let (latest_event, latest_identity) =
        latest_provider_event_identity(&source, FirstPartyCli::Claude, id)
            .unwrap()
            .unwrap();
    assert_ne!(latest_event, older_event);
    assert_eq!(latest_identity, REOPEN_TEST_CONVERSATION);

    for (label, contents) in [
        ("empty", String::new()),
        (
            "truncated",
            "{\"provider\": \"claude\", \"message\": \"do".to_owned(),
        ),
        ("field-less", "{\"provider\": \"claude\"}".to_owned()),
        (
            "foreign provider",
            serde_json::json!({
                "provider": "codex",
                "message": "done",
                "provider_session_id": "01a0d22a-e41e-7661-b666-229f7f1e6435",
                "turn_id": null,
                "created_unix_ms": 1,
            })
            .to_string(),
        ),
    ] {
        fs::write(&older_path, contents).unwrap();
        let error = inspect_reopen_source(&source, id).unwrap_err();
        assert_eq!(
            reopen_refusal_gate(&error),
            Some("request-unresolved"),
            "{label}: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("request-older"),
            "{label}: {error:#}"
        );
    }

    fs::write(
        &older_path,
        serde_json::json!({
            "provider": "claude",
            "message": "earlier answer",
            "provider_session_id": REOPEN_TEST_CONVERSATION,
            "turn_id": "turn-0",
            "created_unix_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
    let resolved = inspect_reopen_source(&source, id).unwrap();
    assert_eq!(resolved.event_id, latest_event);

    // The newest event, which the receipt of the source's own turn points at, is corrupt:
    // receipt validation runs before identity discovery, so the typed gate is returned.
    let latest_path = source.join("events").join(&latest_event);
    let latest_contents = fs::read(&latest_path).unwrap();
    fs::write(&latest_path, "{\"provider\": \"claude\", \"mess").unwrap();
    let error = inspect_reopen_source(&source, id).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("request-unresolved"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains(&latest_event), "{error:#}");

    // A corrupt newest event that no receipt points at (a legacy record) still refuses under
    // the same gate from identity discovery itself.
    fs::write(&latest_path, latest_contents).unwrap();
    let legacy_path = source.join("events").join("event-9-9.json");
    fs::write(&legacy_path, "").unwrap();
    let error = inspect_reopen_source(&source, id).unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("request-unresolved"),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("event-9-9.json"), "{error:#}");
    fs::remove_file(&legacy_path).unwrap();
    assert!(inspect_reopen_source(&source, id).is_ok());

    // An older legacy record that no receipt points at is validated too, even though the
    // newest usable identity is found in a later event: identity discovery reads every
    // recorded event rather than stopping at the newest one it can use.
    let older_legacy_path = source.join("events").join("event-0-0.json");
    for (label, contents) in [
        ("empty", ""),
        ("truncated", "{\"provider\": \"claude\", \"mess"),
    ] {
        fs::write(&older_legacy_path, contents).unwrap();
        assert_eq!(
            requests::list(&source)
                .unwrap()
                .receipts
                .iter()
                .filter(|receipt| receipt.event_file == "event-0-0.json")
                .count(),
            0,
            "{label}: no receipt points at the legacy record"
        );
        let error = inspect_reopen_source(&source, id).unwrap_err();
        assert_eq!(
            reopen_refusal_gate(&error),
            Some("request-unresolved"),
            "{label}: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("event-0-0.json"),
            "{label}: {error:#}"
        );
        let error = latest_provider_event_identity(&source, FirstPartyCli::Claude, id).unwrap_err();
        assert_eq!(
            reopen_refusal_gate(&error),
            Some("request-unresolved"),
            "{label}: {error:#}"
        );
    }
    // A readable older event without an identity leaves the newest usable identity in place.
    fs::write(
        &older_legacy_path,
        serde_json::json!({
            "provider": "claude",
            "message": "the first turn failed",
            "provider_session_id": null,
            "turn_id": null,
            "created_unix_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
    let resolved = inspect_reopen_source(&source, id).unwrap();
    assert_eq!(resolved.event_id, latest_event);
    assert_eq!(resolved.provider_session_id, REOPEN_TEST_CONVERSATION);
    fs::remove_file(&older_legacy_path).unwrap();
}

#[test]
fn reopen_refusal_record_carries_the_gate_and_only_a_holder_check_failure_closes_the_surface() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(read_reopen_refusal_gate(directory.path()), None);
    let error = record_reopen_refusal(
        directory.path(),
        REOPEN_LAUNCH_GATE,
        "a holder registered after the initial scan".to_owned(),
    );
    assert_eq!(reopen_refusal_gate(&error), Some("provider-unsupported"));
    assert!(
        format!("{error:#}").starts_with("reopen refused (provider-unsupported): a holder"),
        "{error:#}"
    );
    assert_eq!(
        read_reopen_refusal_gate(directory.path()).as_deref(),
        Some("provider-unsupported")
    );
    let record: RecordedReopenRefusal =
        read_json(&directory.path().join(REOPEN_REFUSAL_FILE)).unwrap();
    assert_eq!(record.schema, 2);
    assert_eq!(record.phase, "launch");
    assert_eq!(record.detail, "a holder registered after the initial scan");

    // Only a launch-phase record of the current schema names a gate: a record of another
    // phase, or one without a phase, is not a launch refusal.
    for (label, record) in [
        (
            "follow-up phase",
            serde_json::json!({
                "schema": 2,
                "phase": "follow-up",
                "gate": "reopen-conflict",
                "detail": "held by pid 4242",
                "created_unix_ms": 1,
            }),
        ),
        (
            "schema without a phase",
            serde_json::json!({
                "schema": 1,
                "gate": "reopen-conflict",
                "detail": "held by pid 4242",
                "created_unix_ms": 1,
            }),
        ),
    ] {
        write_json_atomic(&directory.path().join(REOPEN_REFUSAL_FILE), &record).unwrap();
        assert_eq!(read_reopen_refusal_gate(directory.path()), None, "{label}");
    }
    fs::remove_file(directory.path().join(REOPEN_REFUSAL_FILE)).unwrap();
    let error = record_reopen_refusal(
        directory.path(),
        REOPEN_LAUNCH_GATE,
        "a holder registered after the initial scan".to_owned(),
    );

    // A non-conflict failure reaches the caller unchanged and touches no session state.
    let before = snapshot_directory(directory.path());
    let passed = close_surface_after_reopen_verification_failure(
        directory.path(),
        "session-reopenrefuse",
        anyhow::anyhow!("delivery failed"),
    );
    assert_eq!(format!("{passed:#}"), "delivery failed");
    let passed = close_surface_after_reopen_verification_failure(
        directory.path(),
        "session-reopenrefuse",
        error,
    );
    assert_eq!(reopen_refusal_gate(&passed), Some("provider-unsupported"));
    assert_eq!(snapshot_directory(directory.path()), before);

    // A session without a resumed_from has no conversation to check at either boundary.
    for check in [
        ResumedHolderCheck::AfterLaunch,
        ResumedHolderCheck::BeforeInitialDelivery,
    ] {
        verify_reopened_conversation_exclusive(
            FirstPartyCli::Claude,
            directory.path(),
            None,
            Instant::now() + Duration::from_secs(1),
            check,
        )
        .unwrap();
    }
    assert_eq!(snapshot_directory(directory.path()), before);

    // The close outcome is reported only after it is known; the detected refusal itself
    // never claims a closure.
    for gate in [REOPEN_CONFLICT_GATE, REOPEN_VERIFICATION_FAILED_GATE] {
        let detected = reopen_refusal(gate, "detected; no prompt was delivered".to_owned());
        let mut closed_with = None;
        let reported = close_surface_after_reopen_verification_failure_with(
            "session-reopenrefuse",
            detected,
            |detected| {
                closed_with = Some(format!("{detected:#}"));
                Ok(())
            },
        );
        assert_eq!(
            closed_with.as_deref(),
            Some(format!("reopen refused ({gate}): detected; no prompt was delivered").as_str())
        );
        assert_eq!(reopen_refusal_gate(&reported), Some(gate));
        assert!(
            format!("{reported:#}").starts_with(
                "the reopened session session-reopenrefuse was closed before any prompt was delivered: reopen refused"
            ),
            "{reported:#}"
        );
        let detected = reopen_refusal(gate, "detected; no prompt was delivered".to_owned());
        let reported = close_surface_after_reopen_verification_failure_with(
            "session-reopenrefuse",
            detected,
            |_| Err(anyhow::anyhow!("terminal adapter unavailable")),
        );
        assert_eq!(reopen_refusal_gate(&reported), Some(gate));
        let text = format!("{reported:#}");
        assert!(text.starts_with("the reopened session session-reopenrefuse could not be closed and may still hold the conversation: terminal adapter unavailable"), "{text}");
        assert!(!text.contains("was closed"), "{text}");
    }
}

// Marker claim, session creation, and an explicit close of the source are interleaved in
// every order through a command channel; each order admits exactly one reopen, refuses the
// rival, and leaves the source closed with its tombstone.
#[test]
fn reopen_lifecycle_interleavings_each_admit_one_outcome_and_keep_the_source_tombstone() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Step {
        Claim,
        Create,
        Finalize,
        Close,
        RivalClaim,
    }
    use Step::*;
    let orders: [[Step; 5]; 5] = [
        [Claim, Close, RivalClaim, Create, Finalize],
        [Close, Claim, Create, RivalClaim, Finalize],
        [Claim, Create, Close, Finalize, RivalClaim],
        [Claim, Create, Finalize, Close, RivalClaim],
        [RivalClaim, Claim, Close, Create, Finalize],
    ];
    for (index, order) in orders.iter().enumerate() {
        let root = tempfile::tempdir().unwrap();
        let source_id = format!("session-reopenlc{index}");
        let new_id = format!("session-reopenlcnew{index}");
        let source = write_closed_reopen_source(
            root.path(),
            &source_id,
            "claude",
            Some(REOPEN_TEST_CONVERSATION),
            true,
        );
        let before = snapshot_directory(&source);

        // The reopening thread executes only the step it is handed and reports back, so the
        // main thread decides the exact order of every boundary.
        let (command_sender, command_receiver) = std::sync::mpsc::channel::<Step>();
        let (report_sender, report_receiver) = std::sync::mpsc::channel::<Result<(), String>>();
        let reopener = {
            let source = source.clone();
            let source_id = source_id.clone();
            let new_id = new_id.clone();
            let root = root.path().to_owned();
            thread::spawn(move || {
                let mut claim: Option<ReopenMarkerClaim> = None;
                for step in command_receiver {
                    let outcome = match step {
                        Claim => claim_reopen_marker(&source, &source_id, REOPEN_TEST_CONVERSATION)
                            .map(|held| claim = Some(held)),
                        Create => (|| {
                            let new = root.join(&new_id);
                            fs::create_dir(&new)?;
                            fs::create_dir(new.join("events"))?;
                            write_reopen_test_manifest(
                                &new,
                                &new_id,
                                "claude",
                                PathBuf::from("/opt/claude"),
                                root.clone(),
                                false,
                            );
                            record_resumed_from(
                                &new,
                                &read_manifest(&new)?,
                                &ResumedFrom {
                                    session: source_id.clone(),
                                    provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
                                    event_id: "event-1-1.json".to_owned(),
                                },
                            )
                        })(),
                        Finalize => claim
                            .take()
                            .expect("finalize follows a successful claim")
                            .finalize(&new_id),
                        Close | RivalClaim => unreachable!("main-thread steps"),
                    };
                    report_sender
                        .send(outcome.map_err(|error| format!("{error:#}")))
                        .unwrap();
                }
            })
        };

        let mut rival_refusals = 0;
        for step in order {
            match step {
                Claim | Create | Finalize => {
                    command_sender.send(*step).unwrap();
                    report_receiver
                        .recv()
                        .unwrap()
                        .unwrap_or_else(|error| panic!("order {index} {step:?}: {error}"));
                }
                Close => {
                    let mut close_calls = 0;
                    close_session_state(&source, |_| {
                        close_calls += 1;
                        Ok(terminal::CloseOutcome::Closed)
                    })
                    .unwrap();
                    assert_eq!(
                        close_calls, 0,
                        "order {index}: a closed source has no surface"
                    );
                }
                RivalClaim => {
                    match claim_reopen_marker(&source, &source_id, REOPEN_TEST_CONVERSATION) {
                        Ok(rival) => {
                            // Only a rival that runs before the claim can win; it releases
                            // its hold immediately so the scripted claim still proceeds.
                            assert_eq!(order[0], RivalClaim, "order {index}");
                            drop(rival);
                        }
                        Err(error) => {
                            assert_eq!(
                                reopen_refusal_gate(&error),
                                Some("already-reopened"),
                                "order {index}: {error:#}"
                            );
                            rival_refusals += 1;
                        }
                    }
                }
            }
        }
        drop(command_sender);
        reopener.join().unwrap();
        if order[0] != RivalClaim {
            assert_eq!(rival_refusals, 1, "order {index}");
        }

        let marker: ReopenMarker = read_json(&source.join(REOPEN_MARKER_FILE)).unwrap();
        assert_eq!(
            marker.reopened_by.as_deref(),
            Some(new_id.as_str()),
            "order {index}"
        );
        assert_eq!(
            read_resumed_from(&root.path().join(&new_id))
                .unwrap()
                .unwrap()
                .session,
            source_id
        );
        let refused =
            claim_reopen_marker(&source, &source_id, REOPEN_TEST_CONVERSATION).unwrap_err();
        assert_eq!(reopen_refusal_gate(&refused), Some("already-reopened"));
        assert!(source.join(CLOSED_STATUS_FILE).is_file(), "order {index}");
        let tombstone: serde_json::Value =
            read_json(&source.join(TERMINAL_TOMBSTONE_FILE)).unwrap();
        assert_eq!(tombstone["consumed"], true, "order {index}");
        assert_eq!(
            read_json::<SessionStatus>(&source.join("status.json"))
                .unwrap()
                .state,
            "closed"
        );
        let mut after = snapshot_directory(&source);
        after.remove(REOPEN_MARKER_FILE).unwrap();
        // An explicit close of an already closed source rewrites only its tombstone.
        let mut expected = before.clone();
        after.remove(TERMINAL_TOMBSTONE_FILE);
        expected.remove(TERMINAL_TOMBSTONE_FILE);
        assert_eq!(after, expected, "order {index}");
    }
}

fn write_reopen_launch_session(root: &Path, id: &str, provider: &Path) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let workspace = root.join(format!("{id}-workspace"));
    fs::create_dir(&workspace).unwrap();
    write_reopen_test_manifest(
        &directory,
        id,
        "claude",
        provider.to_owned(),
        workspace,
        false,
    );
    record_resumed_from(
        &directory,
        &read_manifest(&directory).unwrap(),
        &ResumedFrom {
            session: "session-reopensrc9".to_owned(),
            provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
            event_id: "event-1-1.json".to_owned(),
        },
    )
    .unwrap();
    write_private(
        &directory.join("initial-prompt.txt"),
        native_delegation_prompt("parent", "review this again").as_bytes(),
    )
    .unwrap();
    update_status(&directory, "launching", None, None).unwrap();
    directory
}

#[cfg(windows)]
fn write_live_registry_entry(registry: &Path, pid: u32, name: &str) {
    let identity = terminal::windows_process_identity(pid).unwrap();
    fs::write(
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
}

// The read-only gate passed on an empty registry; a live holder registered before the
// launch wrapper reached its spawn boundary. The recheck refuses under the same gate, no
// provider process is started, and the record carries the gate to the reopen command.
#[cfg(windows)]
#[test]
fn resumed_session_launch_refuses_before_spawn_when_a_holder_registered_after_the_scan() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    fs::create_dir(&registry).unwrap();
    provider::override_claude_session_registry_for_test(Some(registry.clone()));
    let provider_path = root.path().join("claude.cmd");
    write_private(
        &provider_path,
        b"@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo 2.1.281\r\n  exit /b 0\r\n)\r\necho %*> \"%AGENT_BRIDGE_NATIVE_SESSION_DIR%\\argv.txt\"\r\n",
    )
    .unwrap();
    // The initial read-only scan sees nobody.
    provider::verify_reopen_available(FirstPartyCli::Claude, REOPEN_TEST_CONVERSATION).unwrap();
    // A foreign resume registers between that scan and the launch boundary.
    write_live_registry_entry(&registry, std::process::id(), "foreign-resume");

    let id = "session-reopenrecheck";
    let directory = write_reopen_launch_session(root.path(), id, &provider_path);
    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    let error = result.unwrap_err();
    assert_eq!(reopen_refusal_gate(&error), Some("provider-unsupported"));
    assert!(
        format!("{error:#}").contains(&format!("live Claude Code process {}", std::process::id())),
        "{error:#}"
    );
    assert!(
        !directory.join("argv.txt").exists(),
        "the provider was spawned"
    );
    assert!(directory.join("initial-prompt.txt").is_file());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "failed");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .starts_with("reopen refused (provider-unsupported)"),
        "{status:?}"
    );
    assert_eq!(
        read_reopen_refusal_gate(&directory).as_deref(),
        Some("provider-unsupported")
    );
    provider::override_claude_session_registry_for_test(None);
}

// After launch the reopened process has registered under its managed name; a second live
// holder of the same conversation is a detected conflict that fails the new session under
// its own gate before any prompt is delivered.
#[cfg(windows)]
#[test]
fn post_launch_holder_conflict_fails_the_reopened_session_under_its_own_gate() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    fs::create_dir(&registry).unwrap();
    provider::override_claude_session_registry_for_test(Some(registry.clone()));
    let id = "session-reopenconflict";
    let directory = write_reopen_launch_session(root.path(), id, Path::new("/opt/claude"));
    let resumed_from = read_resumed_from(&directory).unwrap().unwrap();

    // The reopened process (stood in for by this test process) registers alone: exclusive
    // at the post-launch scan and at the delivery re-scan.
    write_live_registry_entry(&registry, std::process::id(), id);
    for check in [
        ResumedHolderCheck::AfterLaunch,
        ResumedHolderCheck::BeforeInitialDelivery,
    ] {
        verify_reopened_conversation_exclusive(
            FirstPartyCli::Claude,
            &directory,
            Some(&resumed_from),
            Instant::now() + Duration::from_secs(5),
            check,
        )
        .unwrap();
    }
    assert_eq!(read_reopen_refusal_gate(&directory), None);

    // A foreign live process resumes the same conversation.
    let mut foreign = std::process::Command::new("cmd.exe")
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    write_live_registry_entry(&registry, foreign.id(), "foreign-resume");
    let error = verify_reopened_conversation_exclusive(
        FirstPartyCli::Claude,
        &directory,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        ResumedHolderCheck::AfterLaunch,
    )
    .unwrap_err();
    let _ = foreign.kill();
    let _ = foreign.wait();
    assert_eq!(reopen_refusal_gate(&error), Some("reopen-conflict"));
    assert!(
        format!("{error:#}").contains(&foreign.id().to_string()),
        "{error:#}"
    );
    // Detection is recorded without claiming a closure that has not happened yet.
    assert!(
        format!("{error:#}").ends_with("; no prompt was delivered"),
        "{error:#}"
    );
    assert!(!format!("{error:#}").contains("closed"), "{error:#}");
    assert_eq!(
        read_reopen_refusal_gate(&directory).as_deref(),
        Some("reopen-conflict")
    );
    let record: RecordedReopenRefusal = read_json(&directory.join(REOPEN_REFUSAL_FILE)).unwrap();
    assert!(!record.detail.contains("closed"), "{}", record.detail);
    provider::override_claude_session_registry_for_test(None);
}

// Round 5 reproduction: the initial prompt was delivered and its turn completed while the
// initial messenger was still settling; a later `tell` was refused by its holder check; the
// session was closed; the initial messenger finally reported only that delivery is
// uncertain. The reopen command must not read the `tell` refusal as a refusal of its own
// launch: no launch-phase refusal was recorded, so the response names no gate and the
// source's reopen marker stays consumed. Only a launch-phase record releases it.
#[test]
fn a_later_tell_refusal_never_releases_the_marker_of_a_delivered_initial_prompt() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    fs::create_dir(&registry).unwrap();
    provider::override_claude_session_registry_for_test(Some(registry.clone()));
    let source_id = "session-reopensrc13";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let new_id = "session-reopennew13";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let resumed_from = read_resumed_from(&new).unwrap().unwrap();
    let marker = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    marker.finalize(new_id).unwrap();
    let marker_before = fs::read(source.join(REOPEN_MARKER_FILE)).unwrap();
    let resolve = |id: &str| session_directory_in(root.path(), id);

    // Both launch-phase holder checks passed: the managed process registered alone.
    #[cfg(windows)]
    write_live_registry_entry(&registry, std::process::id(), new_id);
    #[cfg(windows)]
    for check in [
        ResumedHolderCheck::AfterLaunch,
        ResumedHolderCheck::BeforeInitialDelivery,
    ] {
        verify_reopened_conversation_exclusive(
            FirstPartyCli::Claude,
            &new,
            Some(&resumed_from),
            Instant::now() + Duration::from_secs(5),
            check,
        )
        .unwrap();
    }
    assert!(!new.join(REOPEN_REFUSAL_FILE).exists());

    // The initial prompt was delivered and its turn completed.
    fs::remove_file(new.join("initial-prompt.txt")).unwrap();
    for state in ["awaiting-initial-input", "working", "ready"] {
        update_status(&new, state, None, None).unwrap();
    }

    // A later `tell` is refused by its holder check. On native Windows a foreign resume
    // holds the conversation; elsewhere the adapter cannot verify the conversation at all.
    #[cfg(windows)]
    let mut foreign = std::process::Command::new("cmd.exe")
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    #[cfg(windows)]
    write_live_registry_entry(&registry, foreign.id(), "foreign-resume");
    let (mut claim, _) = acquire_ready_turn_claim_with_context(&new, new_id, &[]).unwrap();
    let error = refuse_follow_up_to_shared_conversation(
        FirstPartyCli::Claude,
        &new,
        new_id,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        &mut claim,
        "ready",
    )
    .unwrap_err();
    drop(claim);
    #[cfg(windows)]
    {
        let _ = foreign.kill();
        let _ = foreign.wait();
        assert_eq!(reopen_refusal_gate(&error), Some("reopen-conflict"));
    }
    #[cfg(not(windows))]
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("reopen-verification-failed")
    );
    let status: SessionStatus = read_json(&new.join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
    let tell_reason = status.error.clone().unwrap();
    assert!(tell_reason.contains("reopen refused"), "{tell_reason}");
    // The refusal lives in the status alone: nothing was persisted as a launch refusal.
    assert!(
        !new.join(REOPEN_REFUSAL_FILE).exists(),
        "a follow-up refusal was recorded as a launch refusal"
    );
    assert_eq!(read_reopen_refusal_gate(&new), None);

    // The session is closed with that reason, and only then does the initial messenger
    // report that its delivery could not be confirmed.
    update_status(&new, "closed", None, Some(tell_reason)).unwrap();
    let uncertain = || {
        anyhow::anyhow!(
            "Claude initial cross-session delivery could not be confirmed; the turn remains claimed until completion or explicit close"
        )
    };
    let (outcome, gate) = settle_reopen_outcome(resolve, source_id, new_id, Err(uncertain()));
    assert_eq!(gate, None);
    assert_eq!(
        format!("{:#}", outcome.unwrap_err()),
        format!("{:#}", uncertain())
    );
    assert_eq!(
        fs::read(source.join(REOPEN_MARKER_FILE)).unwrap(),
        marker_before,
        "the reopen marker was released by a follow-up refusal"
    );
    assert!(
        claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).is_err(),
        "a second reopen was admitted"
    );

    // The same uncertain report with a launch-phase refusal recorded in the session is the
    // launch refusal it looks like: the gate is named and the marker is released.
    let recorded = record_reopen_refusal(
        &new,
        REOPEN_CONFLICT_GATE,
        "held by another live process; no prompt was delivered".to_owned(),
    );
    assert_eq!(reopen_refusal_gate(&recorded), Some("reopen-conflict"));
    let (outcome, gate) = settle_reopen_outcome(resolve, source_id, new_id, Err(uncertain()));
    assert_eq!(gate.as_deref(), Some("reopen-conflict"));
    assert_eq!(
        format!("{:#}", outcome.unwrap_err()),
        format!("{:#}", uncertain())
    );
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    provider::override_claude_session_registry_for_test(None);
}

// A refused follow-up records its reason only while the turn is still its own. Between the
// refusal being decided and being recorded, the refused claim can already be gone (a dead
// owner repair, or the pre-fix code that released the claim before writing the reason) and
// a second `tell` can claim the turn and enter `working`. Recording the old refusal then
// must not roll that turn back to `ready`: its claim, its receipt, and its status stay
// intact, and only the refused request's receipt remains unresolved.
#[test]
fn a_refused_follow_up_never_overwrites_the_status_of_the_turn_that_claimed_after_it() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    fs::create_dir(directory.join("events")).unwrap();
    update_status(directory, "ready", None, None).unwrap();
    let (mut refused, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let refused_request = refused.receipt.request_id.clone();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");

    // The refused claim is released out from under the refusal before it is recorded, and
    // the next `tell` claims the turn and starts working.
    release_turn_claim(directory).unwrap();
    update_status(directory, "ready", None, None).unwrap();
    let (next, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let next_token = next.token.clone();
    let next_request = next.receipt.request_id.clone();
    update_status(directory, "working", None, None).unwrap();

    let reported = record_follow_up_refusal("session-test", &mut refused, "ready", refusal);
    drop(refused);
    assert_eq!(
        format!("{reported:#}"),
        "the turn claim of session session-test already belonged to another request; its status was left unchanged: reopen refused (reopen-conflict): held by pid 4242"
    );

    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "working", "{status:?}");
    assert_eq!(status.error, None, "{status:?}");
    assert_eq!(
        fs::read_to_string(directory.join(TURN_CLAIM_FILE))
            .unwrap()
            .trim(),
        next_token
    );
    let receipts = requests::list(directory).unwrap();
    assert_eq!(receipts.unreadable, 0);
    let mut recorded: Vec<&str> = receipts
        .receipts
        .iter()
        .map(|receipt| receipt.request_id.as_str())
        .collect();
    recorded.sort_unstable();
    let mut expected = [refused_request.as_str(), next_request.as_str()];
    expected.sort_unstable();
    assert_eq!(recorded, expected);
    for receipt in &receipts.receipts {
        assert!(
            !directory.join("events").join(&receipt.event_file).exists(),
            "{}: no result was recorded for either request",
            receipt.request_id
        );
    }
    next.retain();
    assert!(directory.join(TURN_CLAIM_FILE).is_file());

    // With the turn still its own, the refusal releases the claim and publishes its reason
    // in the same write.
    release_turn_claim(directory).unwrap();
    update_status(directory, "ready", None, None).unwrap();
    let (mut refused, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");
    let reported = record_follow_up_refusal("session-test", &mut refused, "ready", refusal);
    drop(refused);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "ready", "{status:?}");
    assert_eq!(
        status.error.as_deref(),
        Some(format!("{reported:#}").as_str())
    );
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
}

// The post-launch scan returns as soon as the reopened process has registered. A foreign
// resume that registers after that scan is caught by the re-scan immediately before the
// initial prompt is sent, and the same re-scan refuses a later `tell` while keeping the
// session ready. Detection is per boundary: nothing prevents the registration itself.
#[cfg(windows)]
#[test]
fn a_holder_that_registers_after_the_post_launch_scan_is_caught_before_the_initial_prompt_and_before_a_tell()
 {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    fs::create_dir(&registry).unwrap();
    provider::override_claude_session_registry_for_test(Some(registry.clone()));
    let id = "session-reopenlate2";
    let directory = write_reopen_launch_session(root.path(), id, Path::new("/opt/claude"));
    let resumed_from = read_resumed_from(&directory).unwrap().unwrap();

    // The post-launch scan finds the managed process registered alone and returns.
    write_live_registry_entry(&registry, std::process::id(), id);
    verify_reopened_conversation_exclusive(
        FirstPartyCli::Claude,
        &directory,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        ResumedHolderCheck::AfterLaunch,
    )
    .unwrap();

    // A foreign resume registers only now, after that scan and before the initial delivery.
    let mut foreign = std::process::Command::new("cmd.exe")
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    write_live_registry_entry(&registry, foreign.id(), "foreign-resume");
    let error = verify_reopened_conversation_exclusive(
        FirstPartyCli::Claude,
        &directory,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        ResumedHolderCheck::BeforeInitialDelivery,
    )
    .unwrap_err();
    assert_eq!(reopen_refusal_gate(&error), Some("reopen-conflict"));
    assert!(
        format!("{error:#}").contains(&foreign.id().to_string()),
        "{error:#}"
    );

    // The same session, later ready and being told: the re-scan refuses before delivery,
    // the claim is released with its receipt unresolved, the session stays ready, and the
    // status keeps the reason.
    for state in ["awaiting-initial-input", "working", "ready"] {
        update_status(&directory, state, None, None).unwrap();
    }
    let (mut claim, _) = acquire_ready_turn_claim_with_context(&directory, id, &[]).unwrap();
    let refused_request = claim.receipt.request_id.clone();
    assert_eq!(
        read_json::<SessionStatus>(&directory.join("status.json"))
            .unwrap()
            .state,
        "claimed"
    );
    let launch_record_before = fs::read(directory.join(REOPEN_REFUSAL_FILE)).unwrap();
    let error = refuse_follow_up_to_shared_conversation(
        FirstPartyCli::Claude,
        &directory,
        id,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        &mut claim,
        "ready",
    )
    .unwrap_err();
    assert_eq!(reopen_refusal_gate(&error), Some("reopen-conflict"));
    assert!(
        format!("{error:#}").starts_with(&format!(
            "follow-up to reopened session {id} was refused before delivery: reopen refused (reopen-conflict)"
        )),
        "{error:#}"
    );
    assert!(!directory.join(TURN_CLAIM_FILE).exists());
    // The follow-up refusal is not a launch refusal: the launch record is left as it was.
    assert_eq!(
        fs::read(directory.join(REOPEN_REFUSAL_FILE)).unwrap(),
        launch_record_before
    );
    drop(claim);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(&foreign.id().to_string()),
        "{status:?}"
    );
    let result = query::request_result(&directory, &refused_request).unwrap();
    assert_eq!(result["request_state"], "unresolved", "{result}");
    assert_eq!(result["session_state"], "ready", "{result}");

    // Once the foreign holder is gone, the next delivery is admitted again and the claim
    // is left to the delivery.
    let _ = foreign.kill();
    let _ = foreign.wait();
    let (mut claim, _) = acquire_ready_turn_claim_with_context(&directory, id, &[]).unwrap();
    refuse_follow_up_to_shared_conversation(
        FirstPartyCli::Claude,
        &directory,
        id,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        &mut claim,
        "ready",
    )
    .unwrap();
    assert!(directory.join(TURN_CLAIM_FILE).is_file());
    drop(claim);
    assert!(!directory.join(TURN_CLAIM_FILE).exists());

    // The managed registration disappearing is a verification failure, not exclusivity.
    fs::remove_file(registry.join(format!("{}.json", std::process::id()))).unwrap();
    let (mut claim, _) = acquire_ready_turn_claim_with_context(&directory, id, &[]).unwrap();
    let error = refuse_follow_up_to_shared_conversation(
        FirstPartyCli::Claude,
        &directory,
        id,
        Some(&resumed_from),
        Instant::now() + Duration::from_secs(5),
        &mut claim,
        "ready",
    )
    .unwrap_err();
    drop(claim);
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("reopen-verification-failed"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("no longer registered"),
        "{error:#}"
    );
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state, "ready");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("reopen-verification-failed"),
        "{status:?}"
    );
    provider::override_claude_session_registry_for_test(None);
}

// Every failure to complete the post-launch holder check takes the conflict path: the
// gate is recorded, the initial claim is released with its receipt unresolved, only the new
// surface is closed, and the source's marker is released so it can be reopened again.
#[test]
fn post_launch_verification_failure_closes_only_the_new_surface_and_releases_the_source_marker() {
    let root = tempfile::tempdir().unwrap();
    let unreadable_registry = root.path().join("sessions-as-a-file");
    fs::write(&unreadable_registry, "not a directory").unwrap();
    let empty_registry = root.path().join("sessions-empty");
    fs::create_dir(&empty_registry).unwrap();
    let source_id = "session-reopensrc10";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );

    for (label, registry, expected_detail, close_succeeds) in [
        (
            "unreadable registry, surface closed",
            &unreadable_registry,
            "failed to read the Claude session registry",
            true,
        ),
        (
            "registration timeout, surface closed",
            &empty_registry,
            "did not register",
            true,
        ),
        (
            "unreadable registry, close failed",
            &unreadable_registry,
            "failed to read the Claude session registry",
            false,
        ),
    ] {
        provider::override_claude_session_registry_for_test(Some(registry.clone()));
        let new_id = "session-reopennew10";
        let new = root.path().join(new_id);
        let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
        claim.finalize(new_id).unwrap();
        let source_before = snapshot_directory(&source);
        fs::create_dir(&new).unwrap();
        fs::create_dir(new.join("events")).unwrap();
        write_reopen_test_manifest(
            &new,
            new_id,
            "claude",
            PathBuf::from("/opt/claude"),
            root.path().to_owned(),
            false,
        );
        let resumed_from = ResumedFrom {
            session: source_id.to_owned(),
            provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
            event_id: "event-1-1.json".to_owned(),
        };
        record_resumed_from(&new, &read_manifest(&new).unwrap(), &resumed_from).unwrap();
        write_json_atomic(
            &new.join(TERMINAL_HANDLE_FILE),
            &reopen_test_terminal(new_id),
        )
        .unwrap();
        update_status(&new, "launching", None, None).unwrap();
        let mut initial_claim = acquire_turn_claim(&new).unwrap();
        let request_id = initial_claim.receipt.request_id.clone();
        update_status(&new, "awaiting-initial-input", None, None).unwrap();

        let error = verify_reopened_conversation_exclusive(
            FirstPartyCli::Claude,
            &new,
            Some(&resumed_from),
            Instant::now() + Duration::from_millis(200),
            ResumedHolderCheck::AfterLaunch,
        )
        .unwrap_err();
        assert_eq!(
            reopen_refusal_gate(&error),
            Some("reopen-verification-failed"),
            "{label}: {error:#}"
        );
        if cfg!(windows) {
            assert!(
                format!("{error:#}").contains(expected_detail),
                "{label}: {error:#}"
            );
        }
        assert!(
            format!("{error:#}").ends_with("; no prompt was delivered"),
            "{label}: {error:#}"
        );
        assert_eq!(
            read_reopen_refusal_gate(&new).as_deref(),
            Some("reopen-verification-failed"),
            "{label}"
        );

        // The launch path: the delivery failure is recorded, the surface is closed through
        // the same path a detected conflict takes, and the initial claim is released.
        record_initial_prompt_delivery_failure(&new, &mut initial_claim, false, &error);
        let mut closed_terminals = Vec::new();
        let reported =
            close_surface_after_reopen_verification_failure_with(new_id, error, |detected| {
                if !close_succeeds {
                    anyhow::bail!("terminal adapter unavailable");
                }
                close_session_state_with_error(&new, Some(format!("{detected:#}")), |session| {
                    closed_terminals.push(session.id.clone());
                    Ok(terminal::CloseOutcome::Closed)
                })
            });
        drop(initial_claim);
        assert_eq!(
            reopen_refusal_gate(&reported),
            Some("reopen-verification-failed")
        );
        let status: SessionStatus = read_json(&new.join("status.json")).unwrap();
        if close_succeeds {
            assert_eq!(closed_terminals, [format!("{new_id}-terminal")], "{label}");
            assert!(
                format!("{reported:#}").starts_with(&format!(
                    "the reopened session {new_id} was closed before any prompt was delivered"
                )),
                "{label}: {reported:#}"
            );
            assert_eq!(status.state, "closed", "{label}");
            assert!(
                status
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("reopen refused (reopen-verification-failed)"),
                "{label}: {status:?}"
            );
            assert!(new.join(TERMINAL_TOMBSTONE_FILE).is_file(), "{label}");
            assert!(!new.join(TERMINAL_HANDLE_FILE).exists(), "{label}");
        } else {
            assert!(closed_terminals.is_empty(), "{label}");
            assert!(
                format!("{reported:#}").starts_with(&format!(
                    "the reopened session {new_id} could not be closed and may still hold the conversation"
                )),
                "{label}: {reported:#}"
            );
            assert_eq!(status.state, "failed", "{label}");
            assert!(new.join(TERMINAL_HANDLE_FILE).is_file(), "{label}");
        }
        assert!(
            !new.join(TURN_CLAIM_FILE).exists(),
            "{label}: claim not released"
        );
        // The receipt survives with its outcome unresolved: nothing was delivered.
        let result = query::request_result(&new, &request_id).unwrap();
        assert_eq!(result["request_state"], "unresolved", "{label}: {result}");
        assert_eq!(result["result"], serde_json::Value::Null, "{label}");

        // The source is untouched except for its marker, which the refusal releases.
        let mut source_after = snapshot_directory(&source);
        assert!(
            source_after.remove("reopen.marker.json").is_some(),
            "{label}: marker missing before release"
        );
        let mut expected = source_before.clone();
        expected.remove("reopen.marker.json");
        assert_eq!(source_after, expected, "{label}");
        let outcome =
            release_reopen_marker_after_refusal(&source, &new, Err(anyhow::anyhow!("refused")));
        let outcome = outcome.unwrap_err();
        assert_eq!(format!("{outcome:#}"), "refused", "{label}");
        assert!(!source.join(REOPEN_MARKER_FILE).exists(), "{label}");
        assert_eq!(snapshot_directory(&source), expected, "{label}");
        assert_eq!(
            read_resumed_from(&new).unwrap().as_ref(),
            Some(&resumed_from),
            "{label}: provenance of the refused session is kept"
        );
        // A subsequent reopen of the same source passes its gates and claims the marker.
        provider::override_claude_session_registry_for_test(Some(empty_registry.clone()));
        inspect_reopen_source(&source, source_id).unwrap();
        // The adapter admits the conversation only on native Windows; elsewhere it refuses
        // before consulting the registry, and the marker gates below are platform-neutral.
        let availability =
            provider::verify_reopen_available(FirstPartyCli::Claude, REOPEN_TEST_CONVERSATION);
        if cfg!(windows) {
            availability.unwrap();
        } else {
            assert!(
                format!("{:#}", availability.unwrap_err())
                    .contains("implemented only for native Windows"),
                "{label}"
            );
        }
        let next = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
        drop(next);
        assert!(!source.join(REOPEN_MARKER_FILE).exists(), "{label}");
        fs::remove_dir_all(&new).unwrap();
    }

    // Release is refused while the refused session still accepts prompts, and when the
    // marker names another reopen.
    let new_id = "session-reopennew11";
    let new = root.path().join(new_id);
    fs::create_dir(&new).unwrap();
    write_reopen_test_manifest(
        &new,
        new_id,
        "claude",
        PathBuf::from("/opt/claude"),
        root.path().to_owned(),
        false,
    );
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    update_status(&new, "ready", None, None).unwrap();
    let outcome = release_reopen_marker_after_refusal(&source, &new, Ok(())).unwrap_err();
    assert!(format!("{outcome:#}").contains("is ready"), "{outcome:#}");
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    update_status(&new, "failed", None, Some("refused".to_owned())).unwrap();
    let other = root.path().join("session-reopennew12");
    fs::create_dir(&other).unwrap();
    update_status(&other, "failed", None, Some("refused".to_owned())).unwrap();
    let outcome = release_reopen_marker_after_refusal(&source, &other, Ok(())).unwrap_err();
    assert!(
        format!("{outcome:#}").contains("not the refused session"),
        "{outcome:#}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    release_reopen_marker_after_refusal(&source, &new, Ok(())).unwrap();
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    // Releasing an already released marker is not an error.
    release_reopen_marker_after_refusal(&source, &new, Ok(())).unwrap();
    provider::override_claude_session_registry_for_test(None);
}
