use super::reopen::tests::snapshot_directory;
use super::session::close::compatibility::*;
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "ready");
    assert_eq!(status.error.as_deref(), Some("Pi turn aborted"));
    let error = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap_err();
    assert!(format!("{error:#}").contains("Pi turn aborted"));
}

#[test]
fn monitor_failures_are_journaled_against_the_current_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "failed");
    assert_eq!(status.error.as_deref(), Some("Pi result monitor stopped"));
    let paths = event_paths(directory.path()).unwrap();
    assert_eq!(paths.len(), 1);
    let event: SessionEvent = read_json(&paths[0]).unwrap();
    assert_eq!(event.error.as_deref(), Some("Pi result monitor stopped"));
}

#[test]
fn terminal_status_cannot_regress_and_generation_is_monotonic() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
    let running: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    update_status(directory.path(), SessionState::Exited, Some(0), None).unwrap();
    let exited: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    assert!(exited.generation > running.generation);
    assert!(update_status(directory.path(), SessionState::Ready, None, None).is_err());
    let stable: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(stable.state.as_str(), "exited");
    assert_eq!(stable.generation, exited.generation);
}

#[test]
fn claimed_session_can_finalize_when_the_provider_exits_before_delivery() {
    let directory = tempfile::tempdir().unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();

    finalize_native_session(directory.path(), &Ok(())).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "exited");
    assert_eq!(status.exit_code, Some(0));
}

#[test]
fn native_finalization_is_idempotent_after_a_terminal_status() {
    for state in ["exited", "failed", "closed"] {
        let directory = tempfile::tempdir().unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        if state != "closed" {
            update_status(directory.path(), SessionState::Running, None, None).unwrap();
        }
        match state {
            "exited" => {
                update_status(directory.path(), state.parse().unwrap(), Some(0), None).unwrap()
            }
            "failed" => update_status(
                directory.path(),
                state.parse().unwrap(),
                None,
                Some("provider failed".to_owned()),
            )
            .unwrap(),
            "closed" => {
                update_status(directory.path(), state.parse().unwrap(), None, None).unwrap()
            }
            _ => unreachable!(),
        }
        let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

        finalize_native_session(directory.path(), &Ok(())).unwrap();

        let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(after.state.as_str(), state);
        assert_eq!(after.generation, before.generation);
    }
}

#[test]
fn completion_and_process_exit_converge_in_either_serialized_order() {
    for completion_first in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        update_status(directory.path(), SessionState::Running, None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        let claim_token = claim.token().to_owned();
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
        assert_eq!(status.state.as_str(), "exited");
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert_eq!(
            event_paths(directory.path()).unwrap().len(),
            usize::from(completion_first)
        );
    }
}

#[test]
fn duplicate_provider_turn_cannot_release_a_replacement_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

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
    assert_eq!(status.state.as_str(), "working");
}

#[test]
fn correlated_wait_ignores_other_completed_turns() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token().to_owned();
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
                created_unix_ms: Some(unix_ms()),
            },
        )
        .unwrap();
    }
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
            created_unix_ms: Some(unix_ms()),
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let completed_claim = acquire_turn_claim(directory.path()).unwrap();
    let completed_token = completed_claim.token().to_owned();
    write_event(
        directory.path(),
        &SessionEvent {
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    release_turn_claim(directory.path()).unwrap();
    let later_claim = acquire_turn_claim(directory.path()).unwrap();
    later_claim.retain();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

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
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    write_event(
        directory.path(),
        &SessionEvent {
            provider: "codex".to_owned(),
            message: "completed result".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(unix_ms()),
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
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    for message in ["expected turn", "later turn"] {
        write_event(
            directory.path(),
            &SessionEvent {
                provider: "codex".to_owned(),
                message: message.to_owned(),
                error: None,
                provider_session_id: None,
                turn_id: None,
                created_unix_ms: Some(unix_ms()),
            },
        )
        .unwrap();
    }

    let event = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap();

    assert_eq!(event.message, "expected turn");
}

fn write_resume_wait_owner(directory: &Path, state: &str, pid: u32) {
    update_status(directory, state.parse().unwrap(), None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "closed");
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
    assert_eq!(status.state.as_str(), "closed");
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
        claim.token()
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
        state: state.parse().unwrap(),
        generation: 1,
        updated_unix_ms: closed_unix_ms.unwrap_or(1_000),
        exit_code: None,
        error: None,
        residual_surface: None,
    };
    write_json_atomic(&directory.join("status.json"), &status).unwrap();
    if let Some(updated_unix_ms) = closed_unix_ms {
        write_json_atomic(
            &directory.join(CLOSED_STATUS_FILE),
            &SessionStatus {
                state: SessionState::Closed,
                generation: 1,
                updated_unix_ms,
                exit_code: None,
                error: None,
                residual_surface: None,
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
        SessionState::Failed,
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
    assert_eq!(status.state.as_str(), "closed");
    assert!(
        status.error.is_none(),
        "ordinary launch errors are not retained by handle-less close"
    );
}

#[test]
fn explicit_close_remains_available_with_a_corrupt_completion_journal() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "closed");
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid pending native turn completion"))
    );
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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();

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
            .state
            .as_str(),
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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
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
            .state
            .as_str(),
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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Closed, None, None).unwrap();
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
            .state
            .as_str(),
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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

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
            wezterm_mux: None,
            windows_process_identity: None,
        },
    )
    .unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();

    close_session_state(directory.path(), |_| Ok(terminal::CloseOutcome::Closed)).unwrap();
    update_status(directory.path(), SessionState::Exited, Some(1), None).unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("provider exited after close".to_owned()),
    )
    .unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
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
    assert!(
        terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("set bridgeCommand to item 2 of argv")
    );
    assert!(!terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains("write text"));
    assert!(
        terminal::macos::iterm2::OPEN_TAB_SCRIPT
            .contains("with default profile command bridgeCommand")
    );
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
fn iterm_new_window_returns_selection_before_starting_the_provider() {
    let script = terminal::macos::iterm2::OPEN_TAB_SCRIPT;
    // The new-window path previously remembered nothing: a real self-test
    // selected its window for all 28 seconds and brought iTerm2 to the front.
    assert!(script.contains("set keyboardWindow to my itermCurrentWindow()"));
    assert!(script.contains(
        "set newSessionId to my itermCreateWindow(bridgeCommand)\n        try\n            my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)\n        end try"
    ));
    // iTerm2 activates itself after the creation and then makes the new window
    // key again. The return waits for iTerm2's own word that this has happened,
    // selects the earlier window only while the new session is the selected one,
    // and looks at both again before it gives the foreground back.
    let (_, handler) = script
        .split_once("on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)")
        .unwrap();
    let (handler, _) = handler.split_once("end returnFromNewWindow").unwrap();
    let mut rest = handler;
    for step in [
        "repeat until my itermIsActive()",
        "if looks > activationLooks then return",
        "end repeat\n    if (my itermSelectedSessionId()) is not newSessionId then return",
        "my itermSelectWindow(keyboardWindow)",
        "if not (my isEarlierOrITerm(my foregroundApplication(), earlierApplication)) then return",
        "if earlierApplication's isTerminated() as boolean then return",
        "if not (my itermIsActive()) then return\n    if (my itermSelectedSessionId()) is not expectedSessionId then return\n    earlierApplication's activateWithOptions:2\n",
    ] {
        let (_, after) = rest
            .split_once(step)
            .unwrap_or_else(|| panic!("the return from a new window lost or moved {step:?}"));
        rest = after;
    }
    assert!(rest.trim().is_empty(), "{rest}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_cold_start_never_adopts_an_app_restored_surface() {
    // Closing the key window while iTerm2 is in the background can leave other
    // windows but no current window. That is not authority to adopt any of them.
    // The current window is read once, and only from an iTerm2 that was running.
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains(
        "if my itermIsRunning() then\n        try\n            set keyboardWindow to my itermCurrentWindow()"
    ));
    assert!(terminal::macos::iterm2::OPEN_TAB_SCRIPT.contains(
        "if forceNewWindow or keyboardWindow is missing value then\n        set newSessionId to my itermCreateWindow(bridgeCommand)"
    ));
    assert!(terminal::macos::ghostty::CREATE_SURFACE_SCRIPT.contains(
        "if wantedWindowId is \"-\" then\n   set targetWindow to new window with configuration cfg"
    ));
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
            .contains("set targetWindowId to my windowIdForTty(targetTty, priorWindowIds)")
    );
}

// Issue #58: a new managed session must not take the keyboard. Neither host can
// create a surface without selecting it, so each script gives the keyboard back to
// the surface that had it, only while the new surface still holds it, and never
// returns that surface. The scripts are pinned line by line where it matters: every
// use of the remembered surface and every assignment of the owned one is listed, so
// an added activation, a second use of the remembered surface, or another source
// for the owned surface fails here.
#[cfg(target_os = "macos")]
#[test]
fn macos_open_scripts_give_the_keyboard_back_and_never_activate_the_app() {
    fn lines_with<'a>(script: &'a str, needle: &str) -> Vec<&'a str> {
        script
            .lines()
            .map(str::trim)
            .filter(|line| line.contains(needle))
            .collect()
    }
    let position = |script: &str, needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("open script lost {needle:?}"))
    };

    let iterm = terminal::macos::iterm2::OPEN_TAB_SCRIPT;
    // The comments of the script speak of activation; its statements are listed.
    let iterm_statements = |needle: &str| -> Vec<&str> {
        lines_with(iterm, needle)
            .into_iter()
            .filter(|line| !line.starts_with("--"))
            .collect()
    };
    // iTerm2 is never told to activate. The one activation in the script gives
    // the foreground back to another application, and only the creation of a
    // window leads to it.
    assert_eq!(
        iterm_statements("activate"),
        ["earlierApplication's activateWithOptions:2"]
    );
    assert_eq!(
        iterm_statements("returnFromNewWindow"),
        [
            "on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
            "end returnFromNewWindow",
            "my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
        ]
    );
    let window_return = position(
        iterm,
        "on returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
    );
    let activation = position(iterm, "earlierApplication's activateWithOptions:2");
    assert!(window_return < activation && activation < position(iterm, "end returnFromNewWindow"));
    assert_eq!(
        iterm_statements("returnFromNewTab"),
        [
            "on returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
            "end returnFromNewTab",
            "my returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
        ]
    );
    // A tab is left only while its window still shows it, a window only while
    // its session is the selected one.
    assert_eq!(
        iterm_statements("my itermSelectTab("),
        [
            "if (my itermSessionIdOfWindow(keyboardWindow)) is newSessionId then my itermSelectTab(keyboardTab)"
        ]
    );
    assert_eq!(
        iterm_statements("my itermSelectWindow("),
        ["my itermSelectWindow(keyboardWindow)"]
    );
    // The owned session comes from a creation, its id is read once there, and
    // a tab is created in the window that was remembered.
    assert_eq!(
        iterm_statements("create "),
        [
            "set newWindow to (create window with default profile command bridgeCommand)",
            "set newTab to (create tab with default profile command bridgeCommand)",
        ]
    );
    assert_eq!(
        iterm_statements("unique ID of current session of new"),
        [
            "return unique ID of current session of newWindow",
            "return unique ID of current session of newTab",
        ]
    );
    assert_eq!(
        iterm_statements("set newSessionId to"),
        [
            "set newSessionId to my itermCreateWindow(bridgeCommand)",
            "set newSessionId to my itermCreateTab(keyboardWindow, bridgeCommand)",
        ]
    );
    assert_eq!(
        iterm_statements("set keyboardWindow to"),
        [
            "set keyboardWindow to missing value",
            "set keyboardWindow to my itermCurrentWindow()"
        ]
    );
    assert_eq!(
        iterm_statements("set keyboardTab to"),
        [
            "set keyboardTab to missing value",
            "if keyboardWindow is not missing value then set keyboardTab to my itermCurrentTabOf(keyboardWindow)"
        ]
    );
    let remembered = position(iterm, "set keyboardWindow to my itermCurrentWindow()");
    let created = position(
        iterm,
        "set newSessionId to my itermCreateWindow(bridgeCommand)",
    );
    let window_returned = position(
        iterm,
        "my returnFromNewWindow(earlierApplication, keyboardWindow, newSessionId)",
    );
    let tab_created = position(
        iterm,
        "set newSessionId to my itermCreateTab(keyboardWindow, bridgeCommand)",
    );
    let tab_returned = position(
        iterm,
        "my returnFromNewTab(keyboardWindow, keyboardTab, newSessionId)",
    );
    assert!(
        remembered < created
            && created < window_returned
            && window_returned < tab_created
            && tab_created < tab_returned
    );
    assert!(iterm.ends_with("    return newSessionId\nend run\n"));

    let terminal_app = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(!terminal_app.contains("activate"));
    assert_eq!(
        lines_with(terminal_app, "keyboardWindowId"),
        [
            "set keyboardWindowId to missing value",
            "if (count of windows) > 0 then set keyboardWindowId to id of window 1",
            "if keyboardWindowId is not missing value and keyboardWindowId is not targetWindowId then",
            "set frontmost of (first window whose id is keyboardWindowId and visible is true) to true",
        ]
    );
    assert_eq!(
        lines_with(terminal_app, "set target"),
        [
            "set targetTab to do script ((character id 21) & bridgeCommand)",
            "set targetTty to tty of targetTab",
            "set targetWindowId to my windowIdForTty(targetTty, priorWindowIds)",
            "set targetWindow to first window whose id is targetWindowId",
        ]
    );
    assert_eq!(
        lines_with(terminal_app, "window 1"),
        [
            "if (count of windows) > 0 then set keyboardWindowId to id of window 1",
            "if (id of window 1) is targetWindowId then",
        ]
    );
    let remembered = position(
        terminal_app,
        "try\n                if (count of windows) > 0 then set keyboardWindowId to id of window 1\n            end try",
    );
    let created = position(
        terminal_app,
        "set targetTab to do script ((character id 21) & bridgeCommand)",
    );
    let restored = position(
        terminal_app,
        "try\n                if (id of window 1) is targetWindowId then\n                    set frontmost of (first window whose id is keyboardWindowId and visible is true) to true\n                end if\n            end try",
    );
    assert!(remembered < created && created < restored);
    assert!(terminal_app.ends_with(
        "return targetTty & linefeed & (targetWindowId as text)\n    end tell\nend run\n"
    ));
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

// The Terminal.app ownership decision, executed rather than read: the handler is
// plain AppleScript, so `osascript` runs it without talking to Terminal. The first
// case is the launch that failed on 2026-10-01 (session-y5Wpkl): window 8332 had
// lost its shell while a close confirmation was pending and still reported
// `/dev/ttys003`, the name the new window 8335 received.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_ownership_proof_ignores_windows_that_existed_before_the_launch() {
    const UNPROVEN: &str = "Agent Bridge could not prove the newly created Terminal.app window";
    const DRIVER: &str = r#"
on run argv
    set wantedTty to item 1 of argv
    set priorWindowIds to missing value
    if item 2 of argv is not "unknown" then
        set priorWindowIds to {}
        repeat with priorId in (words of (item 2 of argv))
            set end of priorWindowIds to (priorId as integer)
        end repeat
    end if
    set windowTtys to {}
    repeat with argIndex from 3 to (count of argv) by 2
        set end of windowTtys to {(item argIndex of argv) as integer, item (argIndex + 1) of argv}
    end repeat
    return my soleNewWindowWithTty(windowTtys, priorWindowIds, wantedTty)
end run
"#;
    let script = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    let start = script.find("on soleNewWindowWithTty(").unwrap();
    let end = script.find("end soleNewWindowWithTty").unwrap() + "end soleNewWindowWithTty".len();
    let handler = &script[start..end];
    assert!(
        !handler.contains("tell application"),
        "the decision must not need Terminal"
    );
    let prove = |prior: &str, windows: &[(&str, &str)]| {
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("{handler}\n{DRIVER}"))
            .arg("/dev/ttys003")
            .arg(prior)
            .args(windows.iter().flat_map(|(id, tty)| [*id, *tty]))
            .output()
            .unwrap();
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    };
    let recorded = [("8335", "/dev/ttys003"), ("8332", "/dev/ttys003")];

    // The stale window existed before the launch, so only the new one is a candidate.
    assert_eq!(prove("8332 8316 8329", &recorded), Ok("8335".to_owned()));
    // Without the list of earlier windows the former rule applies: the recorded failure.
    assert!(prove("unknown", &recorded).unwrap_err().contains(UNPROVEN));
    // A cold start has no earlier windows; a restored window with another tty is ignored.
    assert_eq!(
        prove("", &[("10", "/dev/ttys001"), ("11", "/dev/ttys003")]),
        Ok("11".to_owned())
    );
    // The new window is not in the list, or two new windows report the tty: no proof.
    assert!(
        prove("8332", &[("8332", "/dev/ttys003")])
            .unwrap_err()
            .contains(UNPROVEN)
    );
    assert!(
        prove(
            "8332",
            &[("8335", "/dev/ttys003"), ("8336", "/dev/ttys003")]
        )
        .unwrap_err()
        .contains(UNPROVEN)
    );
    // A window that existed before is never returned, even as the only match.
    assert!(
        prove("8332 8335", &recorded)
            .unwrap_err()
            .contains(UNPROVEN),
        "an earlier window must not be adopted"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_app_actions_require_the_recorded_window_and_tty() {
    for script in [
        terminal::macos::apple_terminal::SEND_FILE_SCRIPT,
        terminal::macos::apple_terminal::CLOSE_TAB_SCRIPT,
    ] {
        assert!(script.contains("wantedWindowId"));
        assert!(script.contains("wantedTty"));
    }
    let wait = terminal::macos::apple_terminal::WAIT_FOR_CLOSE_SCRIPT;
    assert!(wait.contains("wantedWindowId"));
    assert!(!wait.contains("wantedTty"));
    // The start is not an action on a recorded tab: Terminal types it into the tab
    // that the same `do script` creates, and no script types a start afterwards.
    let open = terminal::macos::apple_terminal::OPEN_TAB_SCRIPT;
    assert!(open.contains("set targetTab to do script ((character id 21) & bridgeCommand)"));
    assert!(!open.contains("do script \"\""));
    assert!(!open.contains(" in targetTab"));
}

// The tab's shell reaches the gate only through this command: an unrouted name would
// end every Terminal.app launch at its first line.
#[test]
fn terminal_host_command_takes_one_absolute_session_directory() {
    let root = tempfile::tempdir().unwrap();
    let valid_path = root.path().join("session-abc");
    let invalid_path = root.path().join("not-a-session");
    let valid = valid_path.to_str().unwrap();
    let invalid = invalid_path.to_str().unwrap();
    assert!(is_command("native-terminal-host"));
    assert!(matches!(
        parse_args(["native-terminal-host", valid]).unwrap(),
        NativeCommand::AppleTerminalHost { directory } if directory == valid_path
    ));
    for arguments in [
        vec!["native-terminal-host"],
        vec!["native-terminal-host", "state/session-abc"],
        vec!["native-terminal-host", invalid],
        vec!["native-terminal-host", valid, "extra"],
    ] {
        assert!(parse_args(&arguments).is_err(), "{arguments:?}");
    }
}

// The surface's shell reaches the host only through this command. It takes nothing:
// the surface exists before a session is bound to it.
#[test]
fn ghostty_host_command_takes_no_argument() {
    assert!(is_command("native-ghostty-host"));
    assert!(matches!(
        parse_args(["native-ghostty-host"]).unwrap(),
        NativeCommand::GhosttyHost
    ));
    assert!(parse_args(["native-ghostty-host", "/state/session-abc"]).is_err());
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
    fn assert_ordered(script: &str, statements: &[&str]) {
        let mut remaining = script;
        for statement in statements {
            let offset = remaining.find(statement).unwrap_or_else(|| {
                panic!("missing or out-of-order Ghostty ownership check: {statement}")
            });
            remaining = &remaining[offset + statement.len()..];
        }
    }
    assert_ordered(
        terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
        &[
            "if id of w is item 3 of argv then",
            "repeat with t in tabs of w",
            "if id of t is item 2 of argv then",
            "repeat with term in terminals of t",
            "if id of term is item 1 of argv then set matchCount to matchCount + 1",
            "if matchCount is 1 then return \"present\"",
            "if matchCount is 0 then return \"missing\"",
            "error \"Ghostty composite ownership is ambiguous\"",
        ],
    );
    assert_ordered(
        terminal::macos::ghostty::SEND_FILE_SCRIPT,
        &[
            "set wantedTerminalId to item 1 of argv",
            "set wantedTabId to item 2 of argv",
            "set wantedWindowId to item 3 of argv",
            "set targetWindow to first window whose id is wantedWindowId",
            "repeat with candidateTab in tabs of targetWindow",
            "if id of candidateTab is wantedTabId then",
            "if targetTab is missing value then error",
            "repeat with candidateTerminal in terminals of targetTab",
            "if id of candidateTerminal is wantedTerminalId then",
            "if targetTerminal is missing value then error",
            "input text promptText to targetTerminal",
        ],
    );
    assert_ordered(
        terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
        &[
            "set ws to every window whose id is item 3 of argv",
            "if (count of ws) is not 1 then error",
            "set ts to every tab of item 1 of ws whose id is item 2 of argv",
            "if (count of ts) is not 1 then error",
            "set targetTab to item 1 of ts",
            "set terms to every terminal of targetTab whose id is item 1 of argv",
            "if (count of terms) is not 1 then error",
            "if (count of terminals of targetTab) is 1 then",
            "close tab targetTab",
            "else",
            "close (item 1 of terms)",
        ],
    );
    for script in [
        terminal::macos::ghostty::VERIFY_SURFACE_SCRIPT,
        terminal::macos::ghostty::SEND_FILE_SCRIPT,
        terminal::macos::ghostty::CLOSE_TAB_SCRIPT,
    ] {
        assert!(!script.contains("wantedOwnershipTitle"));
        assert!(!script.contains("name of targetTab"));
        assert!(!script.contains("name of targetWindow"));
        assert!(!script.contains("name of targetTerminal"));
        assert!(!script.contains("title"));
        assert!(!script.contains("focused terminal"));
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
            "Ghostty foreground",
            terminal::macos::ghostty::FOREGROUND_SCRIPT,
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
        // iTerm2 scripts address the application by bundle identifier so that a launch
        // finds it while it is not running (#99); the compiler is given the bundle path.
        let script = script
            .replace(
                &format!("tell application \"{application}\""),
                &format!("tell application \"{path}\""),
            )
            .replace(
                "tell application id \"com.googlecode.iterm2\"",
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
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    update_status(directory.path(), SessionState::Running, None, None).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");
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
    // The window lookup is bounded and takes nothing but its time.
    assert!(matches!(
        parse_args(["native-console-control", "window", "session-owner123", "600"]).unwrap(),
        NativeCommand::ConsoleControl {
            action,
            id,
            input_name: None,
            timeout_ms: Some(600),
        } if action == "window" && id == "session-owner123"
    ));
    for invalid in [
        &["window", "session-owner123"][..],
        &["window", "session-owner123", "0"],
        &["window", "session-owner123", "pending-prompt-1.txt", "600"],
    ] {
        let mut arguments = vec!["native-console-control"];
        arguments.extend_from_slice(invalid);
        assert!(parse_args(arguments).is_err(), "{invalid:?}");
    }
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
    assert!(command.contains("cd '/tmp/project; touch nope' && AGENT_BRIDGE_LAUNCH_STDERR_FD=3 AGENT_BRIDGE_LAUNCH_STDOUT_FD=4 AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'"));
    #[cfg(windows)]
    assert!(command.contains("Set-Location -LiteralPath '/tmp/project; touch nope' -ErrorAction Stop; $env:AGENT_BRIDGE_NATIVE_STATE_DIR = '/tmp/state root'; & '/tmp/Agent Bridge/bin' native-session 'session-safe123'"));
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
    assert!(command.ends_with("exit \"$bridge_status\""));
    #[cfg(windows)]
    assert!(command.ends_with("exit $bridgeStatus"));
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

// Windows refuses to replace a file while any other handle to it is open. A launcher
// polls `status.json` and `launch.json` while the wrapper replaces them, so each
// replacement can meet a reader that holds the record for a moment.
#[cfg(windows)]
#[test]
fn a_record_is_replaced_although_a_reader_holds_it_open_for_a_moment() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("status.json");
    write_json_atomic(&path, &serde_json::json!({ "state": "launching" })).unwrap();
    let reader = File::open(&path).unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        drop(reader);
    });

    write_json_atomic(&path, &serde_json::json!({ "state": "running" })).unwrap();

    release.join().unwrap();
    assert_eq!(
        read_json::<serde_json::Value>(&path).unwrap()["state"],
        "running"
    );
}

#[cfg(windows)]
#[test]
fn a_record_that_stays_open_fails_the_replacement_after_a_bounded_wait() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("status.json");
    write_json_atomic(&path, &serde_json::json!({ "state": "launching" })).unwrap();
    let reader = File::open(&path).unwrap();
    let started = Instant::now();

    let error = write_json_atomic(&path, &serde_json::json!({ "state": "running" })).unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(format!("{error:#}").contains("failed to persist"));
    drop(reader);
    assert_eq!(
        read_json::<serde_json::Value>(&path).unwrap()["state"],
        "launching"
    );
    // The temporary record is removed, not left beside the one it could not replace.
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
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
    assert!(SessionState::Ready.accepts_prompt());
    for state in [
        "launching",
        "running",
        "working",
        "exited",
        "failed",
        "closed",
    ] {
        assert!(
            !state.parse::<SessionState>().unwrap().accepts_prompt(),
            "accepted {state}"
        );
    }
}

#[test]
fn every_provider_declares_its_current_follow_up_transport() {
    assert_eq!(
        provider::follow_up_transport(FirstPartyCli::Codex).as_str(),
        "provider-native-queue"
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
fn initial_prompt_failures_distinguish_safe_abort_from_uncertain_delivery() {
    for delivery_started in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        update_status(
            directory.path(),
            SessionState::AwaitingInitialInput,
            None,
            None,
        )
        .unwrap();
        let mut claim = acquire_turn_claim(directory.path()).unwrap();
        if delivery_started {
            update_status(directory.path(), SessionState::Working, None, None).unwrap();
        }

        {
            let delivery_error = &anyhow::anyhow!("terminal delivery failed");
            let _ = claim.settle_delivery(if delivery_started {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_started {
            assert_eq!(status.state.as_str(), "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        } else {
            assert_eq!(status.state.as_str(), "failed");
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
        update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        let (mut claim, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let failure = if delivery_may_have_occurred {
            terminal::TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
                "terminal delivery uncertain"
            ))
        } else {
            terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                "terminal delivery did not start"
            ))
        };

        {
            let delivery_error = failure.error();
            let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        if delivery_may_have_occurred {
            assert_eq!(status.state.as_str(), "working");
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(status.error.as_deref(), Some("terminal delivery uncertain"));
        } else {
            assert_eq!(status.state.as_str(), "ready");
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            assert_eq!(
                status.error.as_deref(),
                Some("terminal delivery did not start")
            );
        }
    }
}

#[test]
fn follow_up_cross_session_uncertainty_keeps_the_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (mut claim, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();

    {
        let _ = claim.settle_delivery(turn::Delivery::Uncertain(
            &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
        ));
    };
    drop(claim);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
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
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (mut delivered, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    // The target completed the delivered turn while its sender was still settling.
    release_turn_claim(directory.path()).unwrap();
    update_status(directory.path(), SessionState::Ready, None, None).unwrap();
    let (newer, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();

    {
        let _ = delivered.settle_delivery(turn::Delivery::Uncertain(&anyhow::anyhow!(
            "late report for the completed turn"
        )));
    };
    drop(delivered);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "claimed");
    assert_eq!(status.error, None);
    assert_eq!(
        current_turn_claim_token(directory.path()).unwrap(),
        Some(newer.token().to_owned())
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
        created_unix_ms: Some(1),
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
    update_status(directory, state.parse().unwrap(), None, None).unwrap();
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
            wezterm_mux: None,
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

#[cfg(not(target_os = "macos"))]
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
    assert_eq!(status.state.as_str(), "closed");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn exited_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("exited");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn failed_dead_native_owner_consumes_terminal_without_adapter_calls() {
    assert_dead_terminal_owner_close_converges("failed");
}

// An owner that passed the live Terminal.app verification has its whole identity recorded.
#[cfg(target_os = "macos")]
fn write_attested_apple_terminal_state(
    directory: &Path,
    state: &str,
    owner_pid: u32,
) -> NativeSessionOwner {
    write_owned_terminal_state(directory, state, owner_pid);
    let owner = NativeSessionOwner {
        pid: owner_pid,
        managed_session_id: Some("session-owner123".to_owned()),
        terminal_tty: Some("/dev/ttys999".to_owned()),
        terminal_tty_device: Some(7),
        process_start_seconds: Some(1_790_000_000),
        process_start_microseconds: Some(42),
        process_group: Some(owner_pid),
        terminal_process_group: Some(owner_pid),
        terminal_app: Some(MacTerminalAppIdentity {
            pid: std::process::id(),
            start_seconds: macos_process_start(std::process::id()).unwrap().unwrap().0,
            start_microseconds: macos_process_start(std::process::id()).unwrap().unwrap().1,
        }),
        ..NativeSessionOwner::default()
    };
    write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner).unwrap();
    owner
}

// The first attempt of an explicit Terminal.app close, up to its failure: the owner and the
// surface were verified, the intent was recorded as the teardown records it, the teardown
// ended the owner (it is already gone here), and the adapter close failed.
#[cfg(target_os = "macos")]
fn fail_apple_terminal_close_after_teardown(directory: &Path, owner: &NativeSessionOwner) {
    let failure = close_session_state_with_error(directory, None, |session| {
        record_terminal_close_intent(directory, "session-owner123", session, owner)?;
        Err(anyhow::anyhow!(
            "Terminal.app automation failed: AppleEvent timed out."
        ))
    })
    .unwrap_err();
    assert!(failure.to_string().contains("timed out"));
}

#[cfg(target_os = "macos")]
fn assert_closed_without_terminal_records(directory: &Path) {
    for record in [
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
        TERMINAL_CLOSE_INTENT_FILE,
    ] {
        assert!(!directory.join(record).exists(), "{record} was kept");
    }
    assert!(directory.join(TERMINAL_TOMBSTONE_FILE).exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
}

// The partial transition of an explicit Terminal.app close: the teardown ended the owner and
// the adapter close failed. The retry must reach the adapter instead of reporting the
// surface closed because the owner is gone.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_after_teardown_reaches_the_adapter() {
    for outcome in [
        terminal::CloseOutcome::Closed,
        terminal::CloseOutcome::Missing,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

        // A later command's repair does not report the surface closed.
        assert!(!repair_dead_native_owner(directory.path()).unwrap());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        // The close that recorded the intent may finish without signalling anything. The
        // first assertion keeps a regression from reaching the Terminal.app probe.
        assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
        assert!(
            verify_terminal_close_authority_with_observations(
                directory.path(),
                "session-owner123",
                &handle,
                || panic!("a matching intent does not need a surface query"),
                |_| {
                    let app = owner.terminal_app.as_ref().unwrap();
                    Ok(Some((app.start_seconds, app.start_microseconds)))
                },
                || Ok(vec![owner.terminal_app.clone().unwrap()]),
            )
            .unwrap()
                == TerminalCloseAuthority::SurfaceOnly
        );

        let mut adapter_calls = 0;
        close_repaired_session_state(directory.path(), |session| {
            adapter_calls += 1;
            assert_eq!(session, &handle);
            Ok(outcome)
        })
        .unwrap();
        assert_eq!(adapter_calls, 1, "the retried close must reach the adapter");
        assert_closed_without_terminal_records(directory.path());
    }
}

// A legacy handle has only window/tty identity. Those names can belong to a new
// Terminal process after a restart; a prior close intent must not authorize it. The
// retry never reaches the adapter: only a window that is proven gone ends it.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_without_app_incarnation_never_reaches_the_adapter() {
    for present in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        owner.terminal_app = None;
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let path = directory.path().join(TERMINAL_HANDLE_FILE);
        fail_apple_terminal_close_after_teardown(directory.path(), &owner);
        let handle: terminal::TerminalSession = read_json(&path).unwrap();
        assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
        let result = close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_observations(
                directory.path(),
                "session-owner123",
                session,
                || Ok(present),
                |_| panic!("no app incarnation is recorded"),
                || Ok(Vec::new()),
            )?;
            assert_eq!(
                authority,
                TerminalCloseAuthority::Absent,
                "a legacy retry reached an adapter without app-incarnation proof"
            );
            Ok(terminal::CloseOutcome::Missing)
        });
        assert_eq!(result.is_ok(), !present, "{result:?}");
        if present {
            assert!(path.exists());
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        } else {
            assert_closed_without_terminal_records(directory.path());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_incarnation_gates_retry_and_read_only_absence() {
    for prior_intent in [false, true] {
        for mode in [
            "same-present",
            "same-absent",
            "dead",
            "reused",
            "unreadable",
            "missing",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
            let birth = (100, 42);
            owner.terminal_app = (mode != "missing").then_some(MacTerminalAppIdentity {
                pid: 1234,
                start_seconds: birth.0,
                start_microseconds: birth.1,
            });
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if prior_intent {
                fail_apple_terminal_close_after_teardown(directory.path(), &owner);
            }
            let observations = std::cell::Cell::new(0);
            let result = close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_observations(
                    directory.path(),
                    "session-owner123",
                    session,
                    || {
                        observations.set(observations.get() + 1);
                        Ok(mode == "same-present")
                    },
                    |pid| {
                        assert_eq!(pid, 1234);
                        match mode {
                            "dead" => Ok(None),
                            "reused" => Ok(Some((101, 42))),
                            "unreadable" => bail!("injected OS observation error"),
                            "missing" => panic!("missing app identity must not query"),
                            _ => Ok(Some(birth)),
                        }
                    },
                    // A record without an incarnation is not settled by a reply that a
                    // second Terminal process could have given.
                    || {
                        let mut instances = vec![terminal_instance(1234, 100)];
                        if mode == "missing" {
                            instances.push(terminal_instance(5678, 200));
                        }
                        Ok(instances)
                    },
                )?;
                if authority == TerminalCloseAuthority::Absent {
                    Ok(terminal::CloseOutcome::Missing)
                } else {
                    assert!(prior_intent);
                    assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
                    bail!("injected adapter failure retains retry")
                }
            });
            let absent = mode == "dead" || (!prior_intent && mode == "same-absent");
            assert_eq!(result.is_ok(), absent, "{prior_intent}/{mode}: {result:?}");
            if absent {
                assert_closed_without_terminal_records(directory.path());
            } else {
                assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
            }
            assert_eq!(
                observations.get(),
                usize::from(!prior_intent && mode.starts_with("same-")),
                "{prior_intent}/{mode}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_app_capture_requires_stable_verified_shell_ancestry() {
    let shell = MacTerminalShellIdentity {
        pid: 4,
        process_group: 4,
        terminal_tty_device: 7,
        process_start_seconds: 100,
        process_start_microseconds: 42,
    };
    for mode in [
        "same",
        "reused-shell",
        "changed-parent",
        "unreadable",
        "foreign-app",
        "wrong-pid",
    ] {
        let mut visits = std::collections::HashMap::new();
        let result = terminal_app_process_with(&shell, |pid| {
            let count = visits.entry(pid).or_insert(0);
            *count += 1;
            if mode == "unreadable" && pid == 3 {
                bail!("injected unreadable ancestor");
            }
            let seconds = if mode == "reused-shell" && pid == 4 {
                101
            } else {
                100
            };
            let parent = if mode == "changed-parent" && *count > 1 && pid == 3 {
                1
            } else {
                pid - 1
            };
            let path = if pid == 2 && mode != "foreign-app" {
                "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"
            } else {
                "/bin/zsh"
            };
            Ok((
                MacTerminalAppIdentity {
                    pid: if mode == "wrong-pid" { 99 } else { pid },
                    start_seconds: seconds,
                    start_microseconds: 42,
                },
                parent,
                path.into(),
            ))
        });
        assert_eq!(result.is_ok(), mode == "same", "{mode}: {result:?}");
        if let Ok(app) = result {
            assert_eq!(app.pid, 2);
            assert_eq!(visits.get(&4), Some(&2));
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_uses_only_proven_read_only_absence() {
    for mode in [
        "no-owner",
        "no-app",
        "live-owner",
        "app-dead",
        "window-absent",
        "present",
        "reused",
        "unreadable",
        "wrong-window",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
        owner.terminal_app = (mode != "no-app").then_some(MacTerminalAppIdentity {
            pid: 1234,
            start_seconds: 100,
            start_microseconds: 42,
        });
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        if mode == "no-owner" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
        if mode == "wrong-window" {
            handle.window_id = Some("9999".into());
        }
        let observations = std::cell::Cell::new(0);
        let result = apple_terminal_startup_absent_with(
            directory.path(),
            &handle,
            |_| mode == "live-owner",
            |_| match mode {
                "app-dead" => Ok(None),
                "reused" => Ok(Some((101, 42))),
                "unreadable" => bail!("injected OS error"),
                _ => Ok(Some((100, 42))),
            },
            || {
                let mut instances = vec![terminal_instance(1234, 100)];
                if matches!(mode, "no-owner" | "no-app") {
                    instances.push(terminal_instance(5678, 200));
                }
                Ok(instances)
            },
            || {
                observations.set(observations.get() + 1);
                Ok(mode != "window-absent")
            },
        );
        assert_eq!(
            matches!(result, Ok(true)),
            matches!(mode, "app-dead" | "window-absent"),
            "{mode}: {result:?}"
        );
        assert_eq!(
            observations.get(),
            usize::from(matches!(mode, "window-absent" | "present")),
            "{mode}"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_retry_failure_or_interruption_keeps_the_intent_and_the_handle() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    fail_apple_terminal_close_after_teardown(directory.path(), &owner);

    close_repaired_session_state(directory.path(), |_| {
        Err(anyhow::anyhow!(
            "Agent Bridge Terminal.app window no longer holds its tab"
        ))
    })
    .unwrap_err();
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready");

    // A closer that stopped after claiming the handle leaves it claimed; repair leaves it
    // and the next close resumes the claim.
    fs::rename(
        directory.path().join(TERMINAL_HANDLE_FILE),
        directory.path().join(TERMINAL_CLOSING_FILE),
    )
    .unwrap();
    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    assert!(directory.path().join(TERMINAL_CLOSING_FILE).exists());
    let mut adapter_calls = 0;
    close_repaired_session_state(directory.path(), |_| {
        adapter_calls += 1;
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(adapter_calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

// A close interrupted after its intent but before its teardown left the owner alive: the
// intent grants nothing, and the original live-owner rules apply.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_while_the_owner_pid_is_alive() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", std::process::id());
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner).unwrap();
    assert!(
        terminal_close_intent_owner(directory.path(), &handle)
            .unwrap()
            .is_some()
    );
    assert!(!terminal_close_resumable(directory.path(), &handle).unwrap());
}

// Warp can close the owned tab (ending its owner) while its dedicated window remains.
// A second close must retain authority to verify the window, not consume the handle
// merely because the owner died during the first requested close.
#[cfg(target_os = "macos")]
#[test]
fn warp_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Warp;
    handle.id = "instance-test".into();
    handle.tab_id = Some("tab-test".into());
    handle.window_id = Some("window-test".into());
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(directory.path(), None, |session| {
        record_terminal_close_intent(directory.path(), "session-owner123", session, &owner)?;
        bail!("managed Warp tab is closed but its dedicated empty window remains")
    })
    .unwrap_err();

    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
    assert!(
        verify_terminal_close_authority(directory.path(), "session-owner123", &handle).unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    close_repaired_session_state(directory.path(), |session| {
        assert_eq!(session, &handle);
        calls += 1;
        Ok(terminal::CloseOutcome::Missing)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

#[cfg(target_os = "macos")]
#[test]
fn wezterm_close_retry_preserves_authority_after_partial_surface_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::WezTerm;
    handle.id = "0".into();
    // As a handle is stored, so that the record keeps whatever scope it was created with.
    handle.wezterm_mux = Some(
        serde_json::from_value(serde_json::json!({
            "socket": "/tmp/owned-wezterm/gui-sock-123",
            "pid": 123,
            "start_seconds": 100,
            "start_microseconds": 1,
        }))
        .unwrap(),
    );
    handle.tab_id = None;
    handle.window_id = None;
    write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    close_session_state_with_error(directory.path(), None, |session| {
        record_terminal_close_intent(directory.path(), "session-owner123", session, &owner)?;
        bail!("managed WezTerm pane is closed but its process remains")
    })
    .unwrap_err();

    assert!(!repair_dead_native_owner(directory.path()).unwrap());
    // Guard the production authority call so a regression never probes a real app.
    assert!(terminal_close_resumable(directory.path(), &handle).unwrap());
    assert!(
        verify_terminal_close_authority(directory.path(), "session-owner123", &handle).unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );
    let mut calls = 0;
    close_repaired_session_state(directory.path(), |session| {
        assert_eq!(session, &handle);
        calls += 1;
        Ok(terminal::CloseOutcome::Missing)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_closed_without_terminal_records(directory.path());
}

#[cfg(target_os = "macos")]
type CloseIntentMutation = fn(&Path, &terminal::TerminalSession, &NativeSessionOwner);

// An invalid intent grants no close authority, but is not evidence that the surface
// disappeared. An owner that died before any explicit close also leaves cleanup unverified.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_close_intent_grants_nothing_unless_it_names_this_handle_and_owner() {
    let record = |directory: &Path, id: &str, handle, owner| {
        record_terminal_close_intent(directory, id, handle, owner).unwrap()
    };
    let cases: [(&str, CloseIntentMutation); 6] = [
        ("no intent", |_, _, _| {}),
        ("corrupt intent", |directory, _, _| {
            fs::write(directory.join(TERMINAL_CLOSE_INTENT_FILE), "{").unwrap()
        }),
        ("another session", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.managed_session_id = Some("session-other456".to_owned());
            record_terminal_close_intent(directory, "session-other456", &handle, owner).unwrap()
        }),
        ("another handle", |directory, handle, owner| {
            let mut handle = handle.clone();
            handle.window_id = Some("1002".to_owned());
            record_terminal_close_intent(directory, "session-owner123", &handle, owner).unwrap()
        }),
        ("another owner", |directory, handle, owner| {
            let mut owner = owner.clone();
            owner.process_start_microseconds = Some(43);
            record_terminal_close_intent(directory, "session-owner123", handle, &owner).unwrap()
        }),
        ("owner without its identity", |directory, handle, owner| {
            let owner = NativeSessionOwner {
                pid: owner.pid,
                managed_session_id: owner.managed_session_id.clone(),
                terminal_tty: owner.terminal_tty.clone(),
                ..NativeSessionOwner::default()
            };
            write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner).unwrap();
            record_terminal_close_intent(directory, "session-owner123", handle, &owner).unwrap()
        }),
    ];
    for (case, mutate) in cases {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        mutate(directory.path(), &handle, &owner);
        assert!(
            !terminal_close_resumable(directory.path(), &handle).unwrap(),
            "{case}"
        );
        assert!(
            !repair_dead_native_owner_with_terminal_close(directory.path(), |_| {
                panic!("{case}: absent or invalid intent grants no adapter authority")
            })
            .unwrap(),
            "{case}: missing close evidence must not become cleanup success"
        );
        assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        if case != "no intent" {
            assert!(directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
        }
        assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready", "{case}");
    }

    // Only a Terminal.app handle can carry the intent.
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Iterm2;
    record(directory.path(), "session-owner123", &handle, &owner);
    assert!(!terminal_close_resumable(directory.path(), &handle).unwrap());
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
        assert_eq!(status.state.as_str(), state);
    }
}

#[test]
fn live_native_session_owner_is_not_repaired() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "working");
}

#[cfg(windows)]
pub(super) fn test_windows_process_identity(pid: u32) -> Option<terminal::WindowsProcessIdentity> {
    terminal::windows_process_identity(pid).ok()
}

#[cfg(not(windows))]
pub(super) fn test_windows_process_identity(_pid: u32) -> Option<terminal::WindowsProcessIdentity> {
    None
}

// The provider process record as the launch wrapper writes it, for a process the test
// controls. Pid 0 stands in for a provider that is verified dead.
pub(super) fn write_provider_process_record(directory: &Path, session: &str, pid: u32) {
    write_json_atomic(
        &directory.join(PROVIDER_PROCESS_FILE),
        &ProviderProcessRecord {
            schema: 1,
            managed_session_id: session.to_owned(),
            pid,
            windows_process_identity: test_windows_process_identity(pid),
            spawned_unix_ms: 1,
        },
    )
    .unwrap();
}

// A process that stays alive until the test ends it: it stands in for a provider process
// that outlived its launch wrapper and its console.
pub(super) fn spawn_surviving_process() -> std::process::Child {
    #[cfg(windows)]
    let mut command = Command::new("cmd.exe");
    #[cfg(windows)]
    command.args(["/c", "pause"]);
    #[cfg(not(windows))]
    let mut command = Command::new("/bin/sh");
    #[cfg(not(windows))]
    command.args(["-c", "read _"]);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn dead_native_session_owner_releases_the_turn_and_closes_state() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "closed");
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    claim.retain();

    close_session_state_with_error(
        directory.path(),
        Some("native session process stopped".to_owned()),
        |_| panic!("a missing terminal handle must not call the adapter"),
    )
    .unwrap();

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
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
    update_status(directory.path(), SessionState::ResumePending, None, None).unwrap();
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
    assert_eq!(status.state.as_str(), "closed");
}

#[test]
fn claimed_and_awaiting_initial_input_dead_owners_respect_surface_cleanup() {
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
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(repair_dead_native_owner(directory.path()).unwrap());
        #[cfg(target_os = "macos")]
        {
            assert!(!repair_dead_native_owner(directory.path()).unwrap());
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_eq!(status.state.as_str(), state);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_eq!(status.state.as_str(), "closed");
        }
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
        wezterm_mux: None,
        windows_process_identity: None,
    };

    let directory = tempfile::tempdir().unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("startup failed".to_owned()),
    )
    .unwrap();
    assert!(
        verify_terminal_close_authority(directory.path(), "session-owner123", &session).unwrap()
            == TerminalCloseAuthority::SurfaceOnly
    );

    let ready_directory = tempfile::tempdir().unwrap();
    update_status(ready_directory.path(), SessionState::Ready, None, None).unwrap();
    assert!(
        verify_terminal_close_authority(ready_directory.path(), "session-owner123", &session)
            .is_err()
    );

    let launching_directory = tempfile::tempdir().unwrap();
    update_status(
        launching_directory.path(),
        SessionState::Launching,
        None,
        None,
    )
    .unwrap();
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
        terminal_app: None,
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
        terminal_app: None,
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

    update_status(&directory, SessionState::Launching, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();
    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    let record: ProviderProcessRecord = read_json(&directory.join(PROVIDER_PROCESS_FILE)).unwrap();
    assert_eq!(record.schema, 1);
    assert_eq!(record.managed_session_id, "session-safe123");
    assert_ne!(record.pid, 0);
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
    assert_eq!(status.state.as_str(), "exited");
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

    update_status(&directory, SessionState::Launching, None, None).unwrap();
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

    update_status(&directory, SessionState::Launching, None, None).unwrap();
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

    update_status(&directory, SessionState::Launching, None, None).unwrap();
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
            created_unix_ms: Some(1),
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token().to_owned();
    claim.retain();
    let pending = sample_completion(&claim_token, "late result");
    write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
    update_status(
        directory.path(),
        SessionState::Closed,
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
    assert_eq!(status.state.as_str(), "closed");
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut stale_claim = acquire_turn_claim(directory.path()).unwrap();
    stale_claim.retain_in_place();
    record_provider_result_for_claim(
        directory.path(),
        FirstPartyCli::Codex,
        "turn A result",
        Some("codex-session".to_owned()),
        Some("codex-turn-a".to_owned()),
        Some(stale_claim.token()),
    )
    .unwrap();
    let replacement = acquire_turn_claim(directory.path()).unwrap();
    replacement.retain();
    update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();

    let failure = delivery_uncertain_failure("turn A paste timed out");
    stale_claim
        .settle_delivery(turn::Delivery::Uncertain(failure.error()))
        .unwrap();

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state.as_str(), "working");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}

#[test]
fn legacy_owner_without_process_identity_is_repaired_only_when_its_pid_is_dead() {
    for (pid, expect_repair) in [(std::process::id(), false), (reaped_child_pid(), true)] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
            status.state.as_str(),
            if expect_repair { "closed" } else { "working" }
        );
    }
}

/// After recovery no journal remains. Temporary files an injected fault left behind (as an
/// abrupt stop would) are best-effort records: observation must ignore them.
fn assert_journal_settled(directory: &Path) {
    assert!(!directory.join(TURN_COMPLETION_FILE).exists());
    query::observe_snapshot(&Reader::open_unchecked(directory)).unwrap();
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
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token().to_owned();
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
        if status.state.as_str() == "working" {
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
        assert_eq!(status.state.as_str(), "ready");
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
        wezterm_mux: None,
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
            assert_eq!(status.state.as_str(), "closed");
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
        assert_eq!(status.state.as_str(), "closed");
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
        update_status(directory.path(), SessionState::Launching, None, None).unwrap();
        update_status(directory.path(), SessionState::Running, None, None).unwrap();
        update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        let mut claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain_in_place();
        update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
                    Some(claim.token()),
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
                TurnEvent::DelayedDeliveryFailure => {
                    let failure = delivery_uncertain_failure("paste timed out");
                    let _ = claim.settle_delivery(turn::Delivery::Uncertain(failure.error()));
                }
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let claim_token = claim.token().to_owned();
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
    assert_eq!(status.state.as_str(), "closed");
}

#[cfg(target_os = "macos")]
fn assert_internal_wait_reports_dead_owner(status_wait: bool) {
    let dead_pid = reaped_child_pid();
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Ghostty,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Warp,
    ] {
        for retained in [
            TERMINAL_HANDLE_FILE,
            TERMINAL_CLOSING_FILE,
            TERMINAL_CLOSE_INTENT_FILE,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let owner = write_attested_apple_terminal_state(directory.path(), "working", dead_pid);
            let handle_path = directory.path().join(TERMINAL_HANDLE_FILE);
            let mut handle: terminal::TerminalSession = read_json(&handle_path).unwrap();
            handle.kind = kind;
            write_json_atomic(&handle_path, &handle).unwrap();
            if retained == TERMINAL_CLOSING_FILE {
                fs::rename(&handle_path, directory.path().join(retained)).unwrap();
            } else if retained == TERMINAL_CLOSE_INTENT_FILE {
                record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner)
                    .unwrap();
            }
            let records = ["status.json", SESSION_OWNER_FILE, TURN_CLAIM_FILE, retained];
            let before: Vec<_> = records
                .iter()
                .map(|name| fs::read(directory.path().join(name)).unwrap())
                .collect();
            // An expired deadline makes the priority deterministic, without timing a live app.
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                wait_for_event_for_turn(directory.path(), 0, None, None, Duration::ZERO)
                    .unwrap_err()
            };
            for (name, bytes) in records.iter().zip(before) {
                assert_eq!(
                    fs::read(directory.path().join(name)).unwrap(),
                    bytes,
                    "{kind:?}/{retained}/{name}"
                );
            }
            assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            assert!(
                format!("{error:#}").contains("no longer running"),
                "{kind:?}/{retained}: {error:#}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(false);
}

#[cfg(target_os = "macos")]
#[test]
fn internal_status_wait_reports_dead_owner_without_consuming_surface() {
    assert_internal_wait_reports_dead_owner(true);
    // Also cover a retained, stale ready status in this internal helper. Current
    // macOS launches pass input as a provider argument and do not call this wait.
    let directory = tempfile::tempdir().unwrap();
    write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
    let error = wait_for_status(
        directory.path(),
        SessionState::Ready,
        Instant::now(),
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("no longer running"),
        "{error:#}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn internal_wait_does_not_call_live_or_unverified_owner_dead() {
    for mode in ["live", "mismatched", "missing"] {
        let directory = tempfile::tempdir().unwrap();
        if mode == "mismatched" {
            write_attested_apple_terminal_state(directory.path(), "working", std::process::id());
        } else {
            write_owned_terminal_state(directory.path(), "working", std::process::id());
        }
        if mode == "missing" {
            fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
        }
        // Headless repair may stop at identity lookup before reaching the new
        // guard. Check directly that a live PID with a different identity is not
        // diagnosed as dead by that guard either.
        require_running_wait_owner(directory.path()).unwrap();
        for status_wait in [false, true] {
            let error = if status_wait {
                wait_for_status(
                    directory.path(),
                    SessionState::Ready,
                    Instant::now(),
                    Duration::ZERO,
                )
                .unwrap_err()
            } else {
                wait_for_event_for_turn(directory.path(), 0, None, None, Duration::ZERO)
                    .unwrap_err()
            };
            assert!(
                !format!("{error:#}").contains("no longer running"),
                "{mode}: {error:#}"
            );
            assert!(directory.path().join(TURN_CLAIM_FILE).exists());
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn internal_event_wait_recovers_completion_before_reporting_dead_owner() {
    for published in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write_attested_apple_terminal_state(directory.path(), "working", reaped_child_pid());
        let handle = fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        let token = fs::read_to_string(directory.path().join(TURN_CLAIM_FILE)).unwrap();
        let pending = sample_completion(token.trim(), "completed before owner exit");
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if published {
            recover_pending_completion(directory.path()).unwrap();
        }
        let event = wait_for_event_for_turn(
            directory.path(),
            0,
            Some("provider-turn"),
            Some(token.trim()),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(event.message, "completed before owner exit");
        assert_eq!(
            fs::read(directory.path().join(TERMINAL_HANDLE_FILE)).unwrap(),
            handle
        );
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(CLOSED_STATUS_FILE).exists());
    }
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
    assert_eq!(status.state.as_str(), "launching");
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
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    let mut event = pending.event.clone();
    event.message = event_message.to_owned();
    write_json_atomic(&event_path, &event).unwrap();
    (request_id, event_path)
}

fn request_state(directory: &Path, request_id: &str) -> (String, String) {
    let value = query::request_result(&Reader::open_unchecked(directory), request_id).unwrap();
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
    update_status(
        &directory,
        SessionState::Closed,
        None,
        Some("closed".to_owned()),
    )
    .unwrap();
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
    assert_eq!(status.state.as_str(), "closed");
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
            update_status(&directory, SessionState::Closed, None, None).unwrap();
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
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
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
        Some(initial.token()),
    )
    .unwrap();
    let (replacement, _) = acquire_ready_turn_claim(directory.path(), "session-test").unwrap();
    let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(before.state.as_str(), "claimed");

    {
        let _ = initial.settle_delivery(turn::Delivery::Uncertain(&anyhow::anyhow!(
            "late initial report"
        )));
    };
    drop(initial);

    let after: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(after.state.as_str(), "claimed");
    assert_eq!(after.error, None);
    assert_eq!(after.generation, before.generation);
    assert_eq!(
        current_turn_claim_token(directory.path()).unwrap(),
        Some(replacement.token().to_owned())
    );
    replacement.retain();
}

#[test]
fn initial_cross_session_uncertainty_keeps_its_own_claim_and_records_its_reason() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let mut initial = acquire_turn_claim(directory.path()).unwrap();

    {
        let _ = initial.settle_delivery(turn::Delivery::Uncertain(
            &anyhow::anyhow!("executed input was not reported").context("delivery unconfirmed"),
        ));
    };
    let token = initial.token().to_owned();
    drop(initial);

    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
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
    assert_eq!(status.state.as_str(), "launching");
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

pub(super) struct CloseFixture {
    root: tempfile::TempDir,
    pub(super) directory: PathBuf,
    request_id: String,
    pending: PendingTurnCompletion,
    event_path: PathBuf,
}

/// A working session under `session-fault` with a held claim, a journaled completion
/// whose event is absent, committed, or mismatched, a terminal handle, and a legacy
/// resume marker: everything an explicit close has to settle.
pub(super) fn seed_close_fixture(journaled_event: JournaledEventState) -> CloseFixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-fault");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
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
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), journal_message);
    pending.event_file = claim.receipt().event_file.clone();
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
    update_status(&other, SessionState::Ready, None, None).unwrap();
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
    let snapshot =
        query::Snapshot::read_within(&Reader::open_unchecked(&directory), size - 1).unwrap();
    let value = snapshot
        .result(
            &Reader::open_unchecked(&directory),
            &query::Selector::Request(request_id.clone()),
        )
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
        created_unix_ms: Some(1),
    }
}

/// A working session with a held claim and no completion yet: what a provider completion
/// commits into. Returns the request address, the claim token, and the receipt's event path.
fn seed_claimed_session(directory: &Path) -> (String, String, PathBuf) {
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(directory);
    update_status(directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let event_path = directory.join("events").join(&claim.receipt().event_file);
    let token = claim.token().to_owned();
    claim.retain();
    (request_id, token, event_path)
}

#[test]
fn timeline_rejects_a_log_change_during_a_consistent_lifecycle_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (request, _, _) = seed_claimed_session(directory.path());
    fs::write(directory.path().join(launch::LOG), "1 before\n").unwrap();
    let error = query::with_snapshot_hook(
        |directory| fs::write(directory.join(launch::LOG), "2 after\n").unwrap(),
        || {
            query::timeline_value(
                &Reader::open_unchecked(directory.path()),
                "session-query",
                Some(&request),
            )
        },
    )
    .unwrap_err();
    assert!(error.is::<query::SnapshotBusy>(), "{error:#}");
}

#[test]
fn timeline_reports_busy_without_repair_when_status_keeps_changing() {
    let directory = tempfile::tempdir().unwrap();
    let (request, _, _) = seed_claimed_session(directory.path());
    let error = query::with_snapshot_retry_window(Duration::ZERO, || {
        query::with_snapshot_hook(
            |directory| update_status(directory, SessionState::Working, None, None).unwrap(),
            || {
                query::timeline_value(
                    &Reader::open_unchecked(directory.path()),
                    "session-query",
                    Some(&request),
                )
            },
        )
    })
    .unwrap_err();
    assert!(error.is::<query::SnapshotBusy>(), "{error:#}");
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(!directory.path().join(TURN_COMPLETION_FILE).exists());
}

#[test]
fn timeline_uses_the_existing_publication_predicate_at_every_completion_boundary() {
    for mutations in 0..=3 {
        let directory = tempfile::tempdir().unwrap();
        let (request, token, event_path) = seed_claimed_session(directory.path());
        let event = SessionEvent {
            provider: "codex".to_owned(),
            message: "same result".to_owned(),
            error: None,
            provider_session_id: Some("thread".to_owned()),
            turn_id: Some("turn".to_owned()),
            created_unix_ms: Some(5),
        };
        let mut pending = PendingTurnCompletion::new(&token, event, None).unwrap();
        pending.event_file = event_path.file_name().unwrap().to_str().unwrap().to_owned();
        write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &pending).unwrap();
        if mutations >= 1 {
            write_pending_completion_event(directory.path(), &pending).unwrap();
        }
        if mutations >= 2 {
            update_status(directory.path(), SessionState::Ready, None, None).unwrap();
        }
        if mutations >= 3 {
            release_turn_claim_token(&directory.path().join(TURN_CLAIM_FILE), &token).unwrap();
        }
        let before = snapshot_directory(directory.path());
        let value = query::timeline_value(
            &Reader::open_unchecked(directory.path()),
            "session-query",
            Some(&request),
        )
        .unwrap();
        assert_eq!(
            value["requests"][0]["request_state"],
            if mutations == 0 {
                "recovery_required"
            } else {
                "completed"
            }
        );
        assert_eq!(value["recovery_required"], true);
        assert_eq!(
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["stage"] == "completion" && e["record_state"] == "observed")
                .count(),
            usize::from(mutations >= 1)
        );
        assert_eq!(snapshot_directory(directory.path()), before);
    }
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
        status: (status.state.to_string(), status.error),
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
                SessionState::Ready,
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
            assert_eq!(pending.status_state.as_str(), "failed", "{label}");
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
    let value = query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap();
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
        assert_eq!(status.state.as_str(), "working", "{label}");
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
    update_status(&other, SessionState::Ready, None, None).unwrap();
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
        update_status(directory, SessionState::Working, None, None).unwrap();
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
    let error =
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap_err();
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
    let error =
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap_err();
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
            update_status(directory, SessionState::Working, None, None).unwrap();
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

    let result = retried(&|| {
        query::request_result(&Reader::open_unchecked(&directory), &request_id).unwrap()
    });
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
        SessionState::Closed,
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
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    // Journal, event, terminal status, and claim release all exist, but the event bytes
    // are compact JSON: the same value, not the journal's canonical write.
    let event_path = directory.join("events").join(&pending.event_file);
    fs::write(&event_path, serde_json::to_vec(&pending.event).unwrap()).unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
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
    update_status(&fixture.directory, state.parse().unwrap(), None, None).unwrap();
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
        if cfg!(target_os = "macos") {
            // A dead owner and unreadable journal prove nothing about its window.
            assert_eq!(listing[0]["state"], state, "{label}: {listing:?}");
            assert!(directory.join(TERMINAL_HANDLE_FILE).exists());
            assert!(!directory.join(TERMINAL_TOMBSTONE_FILE).exists());
            assert!(!directory.join(CLOSED_STATUS_FILE).exists());
            assert!(directory.join(TURN_COMPLETION_FILE).exists());
            continue;
        }
        assert_eq!(listing[0]["state"], "closed", "{label}: {listing:?}");
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "closed", "{label}");
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
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let claim = acquire_turn_claim(&directory).unwrap();
        let claim_token = claim.token().to_owned();
        let request_id = claim.receipt().request_id.clone();
        let event_path = directory.join("events").join(&claim.receipt().event_file);
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
    assert_eq!(status.state.as_str(), "ready");
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
                .is_ok_and(|status| status.state.as_str() == "closed")
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
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let request_id = claim.receipt().request_id.clone();
    let mut pending = sample_completion(claim.token(), "late result");
    pending.event_file = claim.receipt().event_file.clone();
    claim.retain();
    write_json_atomic(&directory.join(TURN_COMPLETION_FILE), &pending).unwrap();
    let event_path = directory.join("events").join(&pending.event_file);
    // The journal's exact bytes at the event path, written without a sync of `events/`.
    fs::write(
        &event_path,
        serde_json::to_vec_pretty(&pending.event).unwrap(),
    )
    .unwrap();
    update_status(&directory, SessionState::Ready, None, None).unwrap();
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

#[test]
fn observed_elapsed_queries_preserve_existing_records_and_timestamp_boundaries() {
    use serde_json::{Value, json};
    for (label, receipt_time, result_time, has_receipt, has_event, failed, expected, reason) in [
        (
            "completed",
            Some(100),
            Some(145),
            true,
            true,
            false,
            Some(45),
            None,
        ),
        (
            "failed",
            Some(100),
            Some(145),
            true,
            true,
            true,
            Some(45),
            None,
        ),
        (
            "pending",
            Some(100),
            None,
            true,
            false,
            false,
            None,
            Some("no_published_result"),
        ),
        (
            "legacy",
            Some(100),
            Some(145),
            false,
            true,
            false,
            None,
            Some("missing_receipt"),
        ),
        (
            "missing receipt time",
            None,
            Some(145),
            true,
            true,
            false,
            None,
            Some("missing_receipt_time"),
        ),
        (
            "missing result time",
            Some(100),
            None,
            true,
            true,
            false,
            None,
            Some("missing_result_time"),
        ),
        (
            "inverted",
            Some(145),
            Some(100),
            true,
            true,
            false,
            None,
            Some("inverted_time"),
        ),
        ("zero", Some(0), Some(0), true, true, false, Some(0), None),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-elapsed");
        fs::create_dir_all(directory.join("events")).unwrap();
        write_test_manifest(&directory);
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let event_path = directory.join("events/event-1.json");
        let receipt_path = directory.join("requests/1-2-3.json");
        if has_receipt {
            let mut receipt = json!({"schema":1, "request_id":"request-elapsed",
                "claim_token":"1-2-3", "event_file":"event-1.json"});
            if let Some(time) = receipt_time {
                receipt["created_unix_ms"] = json!(time);
            }
            fs::create_dir(directory.join("requests")).unwrap();
            write_json_atomic(&receipt_path, &receipt).unwrap();
        }
        if has_event {
            let mut event = json!({"provider":"codex", "message":"original result",
                "provider_session_id":"thread", "turn_id":"turn"});
            if failed {
                event["error"] = json!("provider failed");
            }
            if let Some(time) = result_time {
                event["created_unix_ms"] = json!(time);
            }
            write_json_atomic(&event_path, &event).unwrap();
        } else {
            write_private(&directory.join(TURN_CLAIM_FILE), b"1-2-3").unwrap();
        }
        let paths = [
            directory.join("manifest.json"),
            directory.join("status.json"),
            receipt_path,
            event_path,
            directory.join(TURN_CLAIM_FILE),
        ];
        let before = paths.each_ref().map(|path| fs::read(path).ok());
        let selector = if has_receipt { "--request" } else { "--event" };
        let address = if has_receipt {
            "request-elapsed"
        } else {
            "event-1.json"
        };
        let result = cli_result(
            root.path(),
            &["result", "session-elapsed", selector, address, "--json"],
        );
        assert_eq!(
            result["bridge_observed_elapsed_ms"],
            json!(expected),
            "{label}"
        );
        assert_eq!(
            result["bridge_observed_elapsed_reason"],
            json!(reason),
            "{label}"
        );
        assert_eq!(
            result["request_state"],
            if !has_event {
                "pending"
            } else if failed {
                "failed"
            } else {
                "completed"
            },
            "{label}"
        );
        assert_eq!(
            result["result"],
            if has_event {
                json!("original result")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            result,
            cli_result(
                root.path(),
                &["result", "session-elapsed", selector, address, "--json"]
            ),
            "repeat: {label}"
        );
        let list = cli_result(
            root.path(),
            &["result", "session-elapsed", "--list", "--json"],
        );
        for entry in list["events"]
            .as_array()
            .unwrap()
            .iter()
            .chain(list["requests"].as_array().unwrap())
        {
            assert_eq!(
                entry["bridge_observed_elapsed_ms"],
                json!(expected),
                "list: {label}"
            );
            assert_eq!(
                entry["bridge_observed_elapsed_reason"],
                json!(reason),
                "list: {label}"
            );
        }
        let inspect =
            query::inspect_value(&Reader::open_unchecked(&directory), "session-elapsed").unwrap();
        if has_event {
            assert_eq!(
                inspect["latest_result"]["bridge_observed_elapsed_ms"],
                json!(expected),
                "inspect latest: {label}"
            );
            assert_eq!(
                inspect["latest_result"]["bridge_observed_elapsed_reason"],
                json!(reason),
                "inspect latest: {label}"
            );
        }
        if has_receipt {
            assert_eq!(
                inspect["requests"][0]["bridge_observed_elapsed_ms"],
                json!(expected),
                "inspect request: {label}"
            );
            assert_eq!(
                inspect["requests"][0]["bridge_observed_elapsed_reason"],
                json!(reason),
                "inspect request: {label}"
            );
        }
        assert_eq!(
            before,
            paths.each_ref().map(|path| fs::read(path).ok()),
            "query wrote records: {label}"
        );
    }
}

#[test]
fn observed_elapsed_requires_a_published_result_and_keeps_inspect_available() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-elapsed-boundary");
    fs::create_dir_all(directory.join("events")).unwrap();
    write_test_manifest(&directory);
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let receipt = claim.receipt().clone();
    let mut event = sample_completion(claim.token(), "not published").event;
    event.created_unix_ms = receipt.created_unix_ms;
    write_json_atomic(&directory.join("events").join(&receipt.event_file), &event).unwrap();
    claim.retain();
    let pending =
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(pending["request_state"], "pending");
    assert_eq!(
        pending["bridge_observed_elapsed_ms"],
        serde_json::Value::Null
    );
    assert_eq!(
        pending["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
    fs::remove_file(directory.join("events").join(&receipt.event_file)).unwrap();
    release_turn_claim(&directory).unwrap();
    let unresolved =
        query::request_result(&Reader::open_unchecked(&directory), &receipt.request_id).unwrap();
    assert_eq!(unresolved["request_state"], "unresolved");
    assert_eq!(
        unresolved["bridge_observed_elapsed_reason"],
        "no_published_result"
    );
    // A damaged older result does not prevent inspection of session records or its latest result.
    fs::write(
        directory.join("events").join(&receipt.event_file),
        b"not JSON",
    )
    .unwrap();
    write_json_atomic(
        &directory.join("events/event-99999999999999999999.json"),
        &event,
    )
    .unwrap();
    let inspected = query::inspect_value(
        &Reader::open_unchecked(&directory),
        "session-elapsed-boundary",
    )
    .unwrap();
    assert_eq!(
        inspected["requests"][0]["bridge_observed_elapsed_ms"],
        serde_json::Value::Null
    );
    assert_eq!(
        inspected["requests"][0]["bridge_observed_elapsed_reason"],
        "unreadable_result"
    );
}

#[test]
fn timeline_aba_replacement_keeps_summary_and_entry_together() {
    let directory = tempfile::tempdir().unwrap();
    let (request, token, path) = seed_claimed_session(directory.path());
    release_turn_claim_token(&directory.path().join(TURN_CLAIM_FILE), &token).unwrap();
    let a = oversized_sample_event("A");
    write_json_atomic(&path, &a).unwrap();
    let original = fs::read(&path).unwrap();
    let mut calls = 0;
    let value = query::with_snapshot_hook(
        move |_| {
            calls += 1;
            if calls == 1 {
                let mut b = oversized_sample_event("B");
                b.error = Some("B failure".to_owned());
                write_json_atomic(&path, &b).unwrap();
            } else {
                fs::write(&path, &original).unwrap();
            }
        },
        || {
            query::timeline_value(
                &Reader::open_unchecked(directory.path()),
                "session-query",
                Some(&request),
            )
        },
    )
    .unwrap();
    let completion = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stage"] == "completion")
        .unwrap();
    assert_eq!(value["requests"][0]["error"], completion["detail"]["error"]);
}

#[test]
fn completion_journal_without_result_time_cannot_publish_or_release_claim() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let mut pending = sample_completion(claim.token(), "untimed result");
    pending.event.created_unix_ms = None;
    claim.retain();
    let mut journal = serde_json::to_value(&pending).unwrap();
    journal["event"]
        .as_object_mut()
        .unwrap()
        .remove("created_unix_ms");
    assert!(serde_json::from_value::<PendingTurnCompletion>(journal.clone()).is_ok());
    write_json_atomic(&directory.path().join(TURN_COMPLETION_FILE), &journal).unwrap();
    let error = recover_pending_completion(directory.path()).unwrap_err();
    assert!(format!("{error:#}").contains("missing created_unix_ms"));
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    assert!(directory.path().join(TURN_COMPLETION_FILE).exists());
    assert!(event_paths(directory.path()).unwrap().is_empty());
    let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

#[cfg(target_os = "macos")]
fn assert_dead_owner_preserves_unverified_surface(kind: terminal::TerminalKind) {
    for state in ["ready", "exited", "failed"] {
        for claimed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_attested_apple_terminal_state(directory.path(), state, reaped_child_pid());
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.kind = kind;
            write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
            if claimed {
                fs::rename(
                    directory.path().join(TERMINAL_HANDLE_FILE),
                    directory.path().join(TERMINAL_CLOSING_FILE),
                )
                .unwrap();
            }
            assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
            let result = close_repaired_session_state(directory.path(), |session| {
                verify_terminal_close_authority_with_observations(
                    directory.path(),
                    "session-owner123",
                    session,
                    || Ok(true),
                    |_| {
                        let app = owner.terminal_app.as_ref().unwrap();
                        Ok(Some((app.start_seconds, app.start_microseconds)))
                    },
                    || Ok(vec![owner.terminal_app.clone().unwrap()]),
                )?;
                panic!("a dead owner without intent must grant no adapter authority")
            });
            assert!(
                result.is_err(),
                "{kind:?}/{state}/{claimed}: no adapter proof, but close reported success"
            );
            assert!(
                directory.path().join(TERMINAL_HANDLE_FILE).exists(),
                "{kind:?}: last surface evidence consumed"
            );
            assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            assert_ne!(status.state.as_str(), "closed");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::AppleTerminal);
}
#[cfg(target_os = "macos")]
#[test]
fn warp_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Warp);
}
#[cfg(target_os = "macos")]
#[test]
fn wezterm_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::WezTerm);
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_dead_before_first_close_retains_unverified_surface() {
    assert_dead_owner_preserves_unverified_surface(terminal::TerminalKind::Ghostty);
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_exact_absence_can_finish_without_adapter_or_signal() {
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::Ghostty,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = kind;
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        let mut observations = 0;
        close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_observations(
                directory.path(),
                "session-owner123",
                session,
                || {
                    observations += 1;
                    Ok(false)
                },
                |_| {
                    let app = owner.terminal_app.as_ref().unwrap();
                    Ok(Some((app.start_seconds, app.start_microseconds)))
                },
                || Ok(vec![owner.terminal_app.clone().unwrap()]),
            )?;
            assert_eq!(authority, TerminalCloseAuthority::Absent);
            Ok(terminal::CloseOutcome::Missing)
        })
        .unwrap();
        assert_eq!(observations, 1);
        assert_closed_without_terminal_records(directory.path());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn dead_owner_absence_errors_and_reused_or_incomplete_owners_retain_handle() {
    for mode in ["read-error", "present", "reused", "incomplete", "foreign"] {
        let directory = tempfile::tempdir().unwrap();
        let mut owner =
            write_attested_apple_terminal_state(directory.path(), "exited", reaped_child_pid());
        if mode == "reused" {
            owner.pid = std::process::id();
        }
        if mode == "incomplete" {
            owner.process_start_seconds = None;
        }
        if mode == "foreign" {
            owner.managed_session_id = Some("foreign".to_owned());
        }
        write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
        let mut observed = 0;
        let result = close_repaired_session_state(directory.path(), |session| {
            verify_terminal_close_authority_with_observations(
                directory.path(),
                "session-owner123",
                session,
                || {
                    observed += 1;
                    match mode {
                        "read-error" => bail!("injected unreadable target"),
                        "present" => Ok(true),
                        _ => panic!("unproven owner must not inspect a surface"),
                    }
                },
                |_| {
                    let app = owner.terminal_app.as_ref().unwrap();
                    Ok(Some((app.start_seconds, app.start_microseconds)))
                },
                || Ok(vec![owner.terminal_app.clone().unwrap()]),
            )?;
            panic!("unproven absence must never close or signal")
        });
        assert!(result.is_err(), "{mode}");
        assert_eq!(
            observed,
            usize::from(matches!(mode, "read-error" | "present")),
            "{mode}"
        );
        assert!(
            directory.path().join(TERMINAL_HANDLE_FILE).exists(),
            "{mode}"
        );
        assert!(
            !directory.path().join(TERMINAL_TOMBSTONE_FILE).exists(),
            "{mode}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn warp_close_records_intent_before_stopping_only_attested_group() {
    let directory = tempfile::tempdir().unwrap();
    let owner = write_attested_apple_terminal_state(directory.path(), "ready", 4242);
    let mut handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = terminal::TerminalKind::Warp;
    let live = NativeProcessIdentity {
        pid: 4242,
        parent_pid: 4000,
        terminal_tty_device: 7,
        process_group: 4242,
        terminal_process_group: 4242,
        process_start_seconds: 1_790_000_000,
        process_start_microseconds: 42,
    };
    let shell = NativeProcessIdentity {
        pid: 4000,
        parent_pid: 3000,
        terminal_tty_device: 7,
        process_group: 4000,
        terminal_process_group: 4242,
        process_start_seconds: 1,
        process_start_microseconds: 0,
    };
    let result = prepare_warp_close(
        directory.path(),
        "session-owner123",
        &handle,
        &owner,
        &live,
        &shell,
        |group| {
            assert_eq!(group, 4242);
            assert!(terminal_close_intent_owner(directory.path(), &handle)?.is_some());
            bail!("injected still-running group: adapter must not run")
        },
    );
    assert!(result.unwrap_err().to_string().contains("still-running"));
    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
    fs::remove_file(directory.path().join(TERMINAL_CLOSE_INTENT_FILE)).unwrap();
    let changed = NativeProcessIdentity {
        process_start_microseconds: 43,
        ..live
    };
    assert!(
        prepare_warp_close(
            directory.path(),
            "session-owner123",
            &handle,
            &owner,
            &changed,
            &shell,
            |_| panic!("changed owner must not be signalled")
        )
        .is_err()
    );
    assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn owned_foreground_group_termination_waits_for_real_private_process_exit() {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let group = child.id();
    let waiter = std::thread::spawn(move || child.wait().unwrap());
    let result = terminate_owned_foreground_group(group);
    let status = waiter.join().unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(!status.success());
}

#[cfg(target_os = "macos")]
fn terminal_instance(pid: u32, start_seconds: u64) -> MacTerminalAppIdentity {
    MacTerminalAppIdentity {
        pid,
        start_seconds,
        start_microseconds: 42,
    }
}

// A live process of this test with a controlling TTY of its own (a private pty): what a
// recorded owner PID is once another process has taken it.
#[cfg(target_os = "macos")]
fn spawn_on_private_pty() -> (std::process::Child, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (command.spawn().unwrap(), master)
}

// A launch that failed after its wrapper recorded itself, as `ask`, the wrapper and its
// finalization leave the records: the claim, the launch receipt, the bound handle, the
// complete owner and the failed status. `Pending`: the wrapper failed before the spawn
// (the provider version check). `Spawning`: the spawn is uncertain and keeps the claim.
#[cfg(target_os = "macos")]
fn write_failed_launch_with_owner(
    directory: &Path,
    kind: terminal::TerminalKind,
    phase: launch::Phase,
    owner_pid: u32,
) -> NativeSessionOwner {
    let owner = write_attested_apple_terminal_state(directory, "launching", owner_pid);
    let token = current_turn_claim_token(directory).unwrap().unwrap();
    launch::begin(
        &Store::open_unchecked(directory),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    if phase == launch::Phase::Spawning {
        let mut record = launch::read(&Reader::open_unchecked(directory))
            .unwrap()
            .unwrap();
        record.phase = phase;
        write_json_atomic(&directory.join(launch::FILE), &record).unwrap();
    }
    finalize_native_session(
        directory,
        &Err(anyhow::anyhow!("provider version check failed")),
    )
    .unwrap();
    let mut handle: terminal::TerminalSession =
        read_json(&directory.join(TERMINAL_HANDLE_FILE)).unwrap();
    handle.kind = kind;
    write_json_atomic(&directory.join(TERMINAL_HANDLE_FILE), &handle).unwrap();
    owner
}

// A failed launch is no close intent. A surface that can outlive its owner follows the
// dead-owner rules whatever the launch phase: the whole owner identity, then the read-only
// proof of absence, and never a close or a signal.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_with_a_recorded_owner_gets_no_close_without_a_prior_intent() {
    let (mut other_process, _pty) = spawn_on_private_pty();
    let mut failures = Vec::new();
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in [
                "absent",
                "present",
                "read-error",
                "incomplete",
                "foreign",
                "reused",
            ] {
                let case = format!("{kind:?}/{phase:?}/{mode}");
                let directory = tempfile::tempdir().unwrap();
                let owner_pid = if mode == "reused" {
                    other_process.id()
                } else {
                    reaped_child_pid()
                };
                let mut owner =
                    write_failed_launch_with_owner(directory.path(), kind, phase, owner_pid);
                if mode == "incomplete" {
                    owner.process_start_seconds = None;
                }
                if mode == "foreign" {
                    owner.managed_session_id = Some("session-foreign".to_owned());
                }
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                let app = owner.terminal_app.clone().unwrap();
                let claim = current_turn_claim_token(directory.path()).unwrap();
                assert_eq!(claim.is_some(), phase == launch::Phase::Spawning, "{case}");
                assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());

                let (mut observations, mut adapter_calls) = (0, 0);
                let result = close_repaired_session_state(directory.path(), |session| {
                    let authority = verify_terminal_close_authority_with_observations(
                        directory.path(),
                        "session-owner123",
                        session,
                        || {
                            observations += 1;
                            match mode {
                                "absent" => Ok(false),
                                "present" => Ok(true),
                                _ => bail!("injected unreadable target"),
                            }
                        },
                        |pid| {
                            assert_eq!(pid, app.pid);
                            Ok(Some((app.start_seconds, app.start_microseconds)))
                        },
                        || Ok(vec![app.clone()]),
                    )?;
                    if authority == TerminalCloseAuthority::Absent {
                        return Ok(terminal::CloseOutcome::Missing);
                    }
                    // Where `close_session_surface` signals the owner or calls the adapter.
                    adapter_calls += 1;
                    Ok(terminal::CloseOutcome::Closed)
                });

                if adapter_calls != 0 {
                    failures.push(format!("{case}: {adapter_calls} close/signal step reached"));
                }
                let wanted_observations =
                    usize::from(matches!(mode, "absent" | "present" | "read-error"));
                if observations != wanted_observations {
                    failures.push(format!(
                        "{case}: {observations} surface observations, wanted {wanted_observations}"
                    ));
                }
                let status: SessionStatus =
                    read_json(&directory.path().join("status.json")).unwrap();
                let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
                let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
                if mode == "absent" {
                    if result.is_err()
                        || handle_kept
                        || !tombstone
                        || status.state.as_str() != "closed"
                    {
                        failures.push(format!(
                            "{case}: proven absence did not consume the handle: {result:?}"
                        ));
                    }
                } else if result.is_ok()
                    || !handle_kept
                    || tombstone
                    || status.state.as_str() != "failed"
                    || current_turn_claim_token(directory.path()).unwrap() != claim
                    || launch::read(&Reader::open_unchecked(directory.path()))
                        .unwrap()
                        .unwrap()
                        .phase
                        != phase
                {
                    failures.push(format!(
                        "{case}: handle, claim or state was not retained (result {result:?}, state {})",
                        status.state
                    ));
                }
            }
        }
    }
    other_process.kill().unwrap();
    other_process.wait().unwrap();
    assert!(failures.is_empty(), "{failures:#?}");
}

#[cfg(target_os = "macos")]
#[test]
fn ownerless_warp_and_wezterm_failures_only_consume_proven_absence() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            for mode in ["absent", "present", "unreadable", "foreign"] {
                let directory = tempfile::tempdir().unwrap();
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
                let claim = current_turn_claim_token(directory.path()).unwrap();
                let (mut observations, mut mutations) = (0, 0);
                let result = close_repaired_session_state(directory.path(), |session| {
                    let authority = verify_terminal_close_authority_with_observations(
                        directory.path(),
                        if mode == "foreign" {
                            "session-foreign"
                        } else {
                            "session-owner123"
                        },
                        session,
                        || {
                            observations += 1;
                            match mode {
                                "absent" => Ok(false),
                                "present" => Ok(true),
                                _ => bail!("injected unreadable surface"),
                            }
                        },
                        |_| panic!("these adapters own their process identity checks"),
                        || panic!("this is not a Terminal.app surface"),
                    )?;
                    if authority == TerminalCloseAuthority::Absent {
                        return Ok(terminal::CloseOutcome::Missing);
                    }
                    mutations += 1;
                    Ok(terminal::CloseOutcome::Closed)
                });
                assert_eq!(
                    mutations, 0,
                    "{kind:?}/{phase:?}/{mode}: mutation without owner or intent"
                );
                assert_eq!(observations, usize::from(mode != "foreign"));
                if mode == "absent" {
                    result.unwrap();
                    assert_closed_without_terminal_records(directory.path());
                } else {
                    assert!(result.is_err(), "{kind:?}/{phase:?}/{mode}");
                    assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
                    assert!(!directory.path().join(TERMINAL_TOMBSTONE_FILE).exists());
                    assert_eq!(current_turn_claim_token(directory.path()).unwrap(), claim);
                    assert_eq!(
                        launch::read(&Reader::open_unchecked(directory.path()))
                            .unwrap()
                            .unwrap()
                            .phase,
                        phase
                    );
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn ghostty_failed_start_native_id_recovery_does_not_need_a_prior_intent() {
    for owner_recorded in [false, true] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            let directory = tempfile::tempdir().unwrap();
            write_failed_launch_with_owner(
                directory.path(),
                terminal::TerminalKind::Ghostty,
                phase,
                reaped_child_pid(),
            );
            if !owner_recorded {
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            }
            let handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            assert!(!directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists());
            assert_eq!(
                verify_terminal_close_authority_with_observations(
                    directory.path(),
                    "session-owner123",
                    &handle,
                    || panic!("the exact native-ID close adapter verifies its target"),
                    |_| panic!("no Terminal.app process observation"),
                    || panic!("no Terminal.app instance observation"),
                )
                .unwrap(),
                TerminalCloseAuthority::SurfaceOnly
            );
        }
    }
}

// The close that recorded its intent while the owner was live still finishes after a
// failed launch, and a surface with an app-unique native id keeps its startup recovery.
#[cfg(target_os = "macos")]
#[test]
fn failed_launch_keeps_the_prior_intent_retry_and_the_stable_id_recovery() {
    for kind in [
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Iterm2,
        terminal::TerminalKind::Ghostty,
    ] {
        for phase in [launch::Phase::Pending, launch::Phase::Spawning] {
            let directory = tempfile::tempdir().unwrap();
            let owner =
                write_failed_launch_with_owner(directory.path(), kind, phase, reaped_child_pid());
            let handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            if surface_outlives_owner(kind) {
                record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner)
                    .unwrap();
            }
            let app = owner.terminal_app.clone().unwrap();
            let mut adapter_calls = 0;
            close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_observations(
                    directory.path(),
                    "session-owner123",
                    session,
                    || panic!("{kind:?}/{phase:?}: no surface observation is needed"),
                    |_| Ok(Some((app.start_seconds, app.start_microseconds))),
                    || Ok(vec![app.clone()]),
                )?;
                assert_eq!(
                    authority,
                    TerminalCloseAuthority::SurfaceOnly,
                    "{kind:?}/{phase:?}"
                );
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed)
            })
            .unwrap();
            assert_eq!(adapter_calls, 1, "{kind:?}/{phase:?}");
            assert_closed_without_terminal_records(directory.path());
        }
    }
}

// The production capture on the real ancestors of this test. Under a terminal tab they
// include the root-owned /usr/bin/login, which PROC_PIDTBSDINFO does not answer for.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_reads_the_ancestors_of_any_user() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let owner = NativeSessionOwner {
        pid,
        terminal_shell: Some(MacTerminalShellIdentity {
            pid,
            process_group: 0,
            terminal_tty_device: 0,
            process_start_seconds: seconds,
            process_start_microseconds: microseconds,
        }),
        ..NativeSessionOwner::default()
    };
    match terminal_app_process(&owner) {
        // The suite itself runs in a Terminal.app tab.
        Ok(app) => assert_eq!(
            macos_process_start(app.pid).unwrap(),
            Some((app.start_seconds, app.start_microseconds))
        ),
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!("R2_LIFECYCLE terminal_app_process on this test's lineage: {error}");
            assert!(
                error.contains("has no system Terminal.app ancestor"),
                "an ancestor could not be read: {error}"
            );
        }
    }
}

// The capture's own walk and recheck, with the production observation, up to the
// ancestor that launchd started; that ancestor stands for Terminal.app, which no test
// may start. Under a terminal tab the walk crosses the root-owned /usr/bin/login.
#[cfg(target_os = "macos")]
#[test]
fn terminal_app_capture_walks_this_tests_real_ancestors_to_the_app() {
    let pid = std::process::id();
    let (seconds, microseconds) = macos_process_start(pid).unwrap().unwrap();
    let shell = MacTerminalShellIdentity {
        pid,
        process_group: 0,
        terminal_tty_device: 0,
        process_start_seconds: seconds,
        process_start_microseconds: microseconds,
    };
    let mut lineage = Vec::new();
    let app = terminal_app_process_with(&shell, |pid| {
        let (identity, parent, path) = terminal_app_ancestor(pid)?;
        if !lineage.contains(&path) {
            lineage.push(path.clone());
        }
        let path = if parent == 1 {
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal".to_owned()
        } else {
            path
        };
        Ok((identity, parent, path))
    })
    .unwrap();
    eprintln!("R2_LIFECYCLE real lineage of this test: {lineage:#?}");
    let (identity, parent, _) = terminal_app_ancestor(app.pid).unwrap();
    assert_eq!((identity, parent), (app, 1));
}

// launchd is an ancestor of every process and belongs to root. The record of a process of
// this user carries the same birth and parent as the query that verifies an owner, so an
// incarnation captured here is the one a later close compares.
#[cfg(target_os = "macos")]
#[test]
fn process_records_answer_for_a_root_owned_ancestor_and_agree_with_the_owner_query() {
    let (launchd, parent, path) = terminal_app_ancestor(1).unwrap();
    assert_eq!(
        (launchd.pid, parent, path.as_str()),
        (1, 0, "/sbin/launchd")
    );
    assert!(launchd.start_seconds > 0 && launchd.start_microseconds < 1_000_000);

    let pid = std::process::id();
    let (own, parent, path) = terminal_app_ancestor(pid).unwrap();
    let info = macos_process_info(pid).unwrap().unwrap();
    assert_eq!(
        (own.start_seconds, own.start_microseconds, parent),
        (
            info.process_start_seconds,
            info.process_start_microseconds,
            info.parent_pid
        )
    );
    let executable = std::env::current_exe().unwrap();
    assert_eq!(Path::new(&path).file_name(), executable.file_name());

    // The command name is the executable's, cut to the kernel's 16 bytes.
    let name = executable.file_name().unwrap().as_encoded_bytes();
    let named = macos_processes_named(&name[..name.len().min(16)]).unwrap();
    assert!(named.contains(&own), "{named:?} lacks {own:?}");
    assert!(named.windows(2).all(|pair| pair[0].pid < pair[1].pid));
    assert!(
        macos_processes_named(b"no-such-command")
            .unwrap()
            .is_empty()
    );

    let error = terminal_app_ancestor(reaped_child_pid()).unwrap_err();
    assert!(error.to_string().contains("ended during attestation"));
}

// A Terminal.app record that names no app incarnation: a start that failed before the
// wrapper recorded itself, an owner record of 0.0.10 or earlier, and such a record whose
// close had begun. Nothing ties a reply to the app that created the window, so the window
// is gone only when one stable instance, or none, can have answered and its list lacks
// the window. Everything else keeps the handle, and nothing is ever closed.
#[cfg(target_os = "macos")]
#[test]
fn terminal_record_without_app_incarnation_is_consumed_only_by_proven_absence() {
    let mut failures = Vec::new();
    for shape in [
        "ownerless-failed-start",
        "legacy-dead-owner",
        "legacy-intent-retry",
    ] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "none-running-but-listed",
            "two-instances",
            "restarted",
            "started-meanwhile",
            "listing-error",
            "presence-error",
        ] {
            let case = format!("{shape}/{mode}");
            let directory = tempfile::tempdir().unwrap();
            if shape == "ownerless-failed-start" {
                write_failed_launch_with_owner(
                    directory.path(),
                    terminal::TerminalKind::AppleTerminal,
                    launch::Phase::Pending,
                    reaped_child_pid(),
                );
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            } else {
                let mut owner = write_attested_apple_terminal_state(
                    directory.path(),
                    "ready",
                    reaped_child_pid(),
                );
                owner.terminal_app = None;
                write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
                if shape == "legacy-intent-retry" {
                    fail_apple_terminal_close_after_teardown(directory.path(), &owner);
                }
            }
            let before: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let listings = std::cell::Cell::new(0);
            let (mut observations, mut adapter_calls) = (0, 0);
            let result = close_repaired_session_state(directory.path(), |session| {
                let authority = verify_terminal_close_authority_with_observations(
                    directory.path(),
                    "session-owner123",
                    session,
                    || {
                        observations += 1;
                        match mode {
                            "presence-error" => bail!("injected unreadable window list"),
                            "one-stable-listed" | "none-running-but-listed" => Ok(true),
                            _ => Ok(false),
                        }
                    },
                    |_| panic!("{case}: no app incarnation is recorded"),
                    || {
                        listings.set(listings.get() + 1);
                        let first = listings.get() == 1;
                        match mode {
                            "listing-error" => bail!("injected process table error"),
                            "none-running" | "none-running-but-listed" => Ok(Vec::new()),
                            "two-instances" => Ok(vec![
                                terminal_instance(1234, 100),
                                terminal_instance(5678, 200),
                            ]),
                            "restarted" if !first => Ok(vec![terminal_instance(1234, 101)]),
                            "started-meanwhile" if first => Ok(Vec::new()),
                            _ => Ok(vec![terminal_instance(1234, 100)]),
                        }
                    },
                )?;
                if authority == TerminalCloseAuthority::Absent {
                    return Ok(terminal::CloseOutcome::Missing);
                }
                adapter_calls += 1;
                Ok(terminal::CloseOutcome::Closed)
            });

            let absent = matches!(mode, "none-running" | "one-stable");
            let wanted_observations =
                usize::from(!matches!(mode, "two-instances" | "listing-error"));
            let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
            let handle_kept = directory.path().join(TERMINAL_HANDLE_FILE).exists();
            let tombstone = directory.path().join(TERMINAL_TOMBSTONE_FILE).exists();
            let intent_kept = directory.path().join(TERMINAL_CLOSE_INTENT_FILE).exists();
            let settled = if absent {
                result.is_ok() && !handle_kept && tombstone && status.state.as_str() == "closed"
            } else {
                result.is_err()
                    && handle_kept
                    && !tombstone
                    && status.state == before.state
                    && intent_kept == (shape == "legacy-intent-retry")
            };
            if !settled || adapter_calls != 0 || observations != wanted_observations {
                failures.push(format!(
                    "{case}: result {result:?}, state {}, handle kept {handle_kept}, {adapter_calls} close steps, {observations} observations",
                    status.state
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// The rollback of a failed start asks the same question before the wrapper has recorded
// an owner or an app incarnation. It reads only, and it never touches the binding.
#[cfg(target_os = "macos")]
#[test]
fn apple_terminal_failed_start_without_app_incarnation_needs_one_stable_instance() {
    for shape in ["no-owner", "no-app"] {
        for mode in [
            "none-running",
            "one-stable",
            "one-stable-listed",
            "two-instances",
            "restarted",
            "listing-error",
            "presence-error",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut owner =
                write_attested_apple_terminal_state(directory.path(), "failed", reaped_child_pid());
            owner.terminal_app = None;
            write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
            if shape == "no-owner" {
                fs::remove_file(directory.path().join(SESSION_OWNER_FILE)).unwrap();
            }
            let mut handle: terminal::TerminalSession =
                read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
            handle.managed_session_id = None; // creation-time copy, before bind mutated its peer
            let listings = std::cell::Cell::new(0);
            let result = apple_terminal_startup_absent_with(
                directory.path(),
                &handle,
                |_| false,
                |_| panic!("{shape}/{mode}: no app incarnation is recorded"),
                || {
                    listings.set(listings.get() + 1);
                    match mode {
                        "listing-error" => bail!("injected process table error"),
                        "none-running" => Ok(Vec::new()),
                        "two-instances" => Ok(vec![
                            terminal_instance(1234, 100),
                            terminal_instance(5678, 200),
                        ]),
                        "restarted" if listings.get() > 1 => Ok(vec![terminal_instance(1234, 101)]),
                        _ => Ok(vec![terminal_instance(1234, 100)]),
                    }
                },
                || match mode {
                    "presence-error" => bail!("injected unreadable window list"),
                    "one-stable-listed" => Ok(true),
                    _ => Ok(false),
                },
            );
            match mode {
                "none-running" | "one-stable" => {
                    assert!(matches!(result, Ok(true)), "{shape}/{mode}")
                }
                "one-stable-listed" => assert!(matches!(result, Ok(false)), "{shape}/{mode}"),
                _ => assert!(result.is_err(), "{shape}/{mode}: {result:?}"),
            }
            assert!(directory.path().join(TERMINAL_HANDLE_FILE).exists());
        }
    }
}

// The live owner of a record of 0.0.10 or earlier: the close derives the app incarnation
// from that owner's own ancestry and records it before the intent, which is then exact for
// the updated owner record. A derivation that fails changes nothing.
#[cfg(target_os = "macos")]
#[test]
fn legacy_terminal_owner_records_its_app_incarnation_before_the_close_intent() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = write_attested_apple_terminal_state(directory.path(), "ready", 4242);
    owner.terminal_app = None;
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();
    let handle: terminal::TerminalSession =
        read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
    let stored_app = || {
        read_json::<NativeSessionOwner>(&directory.path().join(SESSION_OWNER_FILE))
            .unwrap()
            .terminal_app
    };

    let error = record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        bail!("the verified terminal shell has no system Terminal.app ancestor")
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no system Terminal.app ancestor")
    );
    assert!(owner.terminal_app.is_none() && stored_app().is_none());

    let app = terminal_instance(1234, 100);
    record_legacy_terminal_app(directory.path(), &mut owner, |legacy| {
        assert_eq!(legacy.pid, 4242);
        Ok(app.clone())
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app.clone()));
    record_terminal_close_intent(directory.path(), "session-owner123", &handle, &owner).unwrap();
    assert_eq!(
        terminal_close_intent_owner(directory.path(), &handle)
            .unwrap()
            .and_then(|owner| owner.terminal_app),
        Some(app.clone())
    );

    record_legacy_terminal_app(directory.path(), &mut owner, |_| {
        panic!("a recorded incarnation is never derived again")
    })
    .unwrap();
    assert_eq!(stored_app(), Some(app));
}

// What a close may touch is the scope its session was created with, as the handle stores
// it. The lifecycle reads no opening preference: the adapter receives the stored handle
// unchanged, and the retry of a close is authorized for that exact handle only, so the
// same target read with another scope is given nothing.
#[cfg(target_os = "macos")]
#[test]
fn close_uses_the_stored_creation_scope_of_a_wezterm_session() {
    for owns_gui in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            write_attested_apple_terminal_state(directory.path(), "ready", reaped_child_pid());
        let mut handle: terminal::TerminalSession =
            read_json(&directory.path().join(TERMINAL_HANDLE_FILE)).unwrap();
        handle.kind = terminal::TerminalKind::WezTerm;
        handle.id = "7".into();
        handle.tab_id = Some("3".into());
        handle.window_id = Some("1".into());
        handle.wezterm_mux = Some(
            serde_json::from_value(serde_json::json!({
                "socket": "/tmp/owned-wezterm/gui-sock-123",
                "pid": 123,
                "start_seconds": 100,
                "start_microseconds": 1,
                "owns_gui": owns_gui,
            }))
            .unwrap(),
        );
        write_json_atomic(&directory.path().join(TERMINAL_HANDLE_FILE), &handle).unwrap();
        close_session_state_with_error(directory.path(), None, |session| {
            assert_eq!(session, &handle);
            record_terminal_close_intent(directory.path(), "session-owner123", session, &owner)?;
            bail!("managed WezTerm pane is closed but its tab remains")
        })
        .unwrap_err();

        let mut other_scope = handle.clone();
        other_scope.wezterm_mux.as_mut().unwrap().owns_gui = !owns_gui;
        assert!(
            terminal_close_intent_owner(directory.path(), &other_scope)
                .unwrap()
                .is_none()
        );
        assert!(
            terminal_close_intent_owner(directory.path(), &handle)
                .unwrap()
                .is_some()
        );

        let mut closed = None;
        close_repaired_session_state(directory.path(), |session| {
            let authority = verify_terminal_close_authority_with_observations(
                directory.path(),
                "session-owner123",
                session,
                || panic!("the recorded intent needs no surface observation"),
                |_| panic!("a WezTerm handle names no Terminal.app incarnation"),
                || panic!("a WezTerm handle lists no Terminal.app instances"),
            )?;
            assert_eq!(authority, TerminalCloseAuthority::SurfaceOnly);
            closed = Some(session.clone());
            Ok(terminal::CloseOutcome::Closed)
        })
        .unwrap();
        assert_eq!(closed, Some(handle));
        assert_closed_without_terminal_records(directory.path());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn attested_terminal_absence_does_not_accept_another_app_instances_reply() {
    let app = terminal_instance(1234, 100);
    let reads = std::cell::Cell::new(0);
    let result = terminal_surface_absent(
        Some(&app),
        |_| Ok(Some((app.start_seconds, app.start_microseconds))),
        || Ok(vec![app.clone(), terminal_instance(5678, 200)]),
        || {
            reads.set(reads.get() + 1);
            Ok(false)
        },
    );
    assert!(
        result.is_err(),
        "ambiguous instance established absence: {result:?}"
    );
    assert_eq!(
        reads.get(),
        0,
        "no app is queried when its address is ambiguous"
    );
}

// An explicit close after a failed startup whose wrapper recorded itself and ended.
// A surface with an app-unique native id (an iTerm2 session, a Windows console) is
// still closed by that id. A macOS surface that can outlive its owner is not: a
// failed launch is no close intent, and an owner record without the whole identity
// does not even allow the observation that could prove the surface gone.
// Close authority is terminal-kind policy, so this test lives at the native boundary and
// not in `session::launch`, which must know nothing about terminal kinds.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn explicit_close_recovers_a_failed_startup_only_through_a_stable_native_id() {
    // The launch fixture: a launching session whose claim the launcher retains and whose
    // launch receipt `begin` wrote, as in `session::launch`'s own tests.
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir()
        .unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Launching, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    launch::begin(
        &Store::open_unchecked(directory.path()),
        &token,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    let id = directory.path().file_name().unwrap().to_str().unwrap();
    let mut record = launch::read(&Reader::open_unchecked(directory.path()))
        .unwrap()
        .unwrap();
    record.phase = launch::Phase::Spawning;
    write_json_atomic(&directory.path().join(launch::FILE), &record).unwrap();
    update_status(
        directory.path(),
        SessionState::Failed,
        None,
        Some("spawn uncertain".to_owned()),
    )
    .unwrap();
    write_json_atomic(
        &directory.path().join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: u32::MAX,
            managed_session_id: Some(id.to_owned()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    let authority = |kind, managed_session_id: &str| {
        verify_terminal_close_authority_with_observations(
            directory.path(),
            id,
            &terminal::TerminalSession {
                kind,
                id: "missing-owned-surface".to_owned(),
                managed_session_id: Some(managed_session_id.to_owned()),
                tab_id: None,
                window_id: None,
                wezterm_mux: None,
                windows_process_identity: None,
            },
            || panic!("an unproven owner allows no surface observation"),
            |_| panic!("no app incarnation is recorded"),
            || panic!("no app incarnation is recorded"),
        )
    };
    assert_eq!(
        authority(terminal::TerminalKind::Iterm2, id).unwrap(),
        TerminalCloseAuthority::SurfaceOnly
    );
    assert!(authority(terminal::TerminalKind::Iterm2, "session-foreign").is_err());
    #[cfg(target_os = "macos")]
    for kind in [
        terminal::TerminalKind::AppleTerminal,
        terminal::TerminalKind::Warp,
        terminal::TerminalKind::WezTerm,
    ] {
        let error = authority(kind, id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no terminal observation or close was sent"),
            "{kind:?}: {error:#}"
        );
    }
    assert!(directory.path().join(TURN_CLAIM_FILE).exists());
}
