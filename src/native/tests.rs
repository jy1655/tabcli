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
    for provider in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
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
    for provider in [FirstPartyCli::Codex, FirstPartyCli::Agy] {
        assert_eq!(
            provider::initial_prompt_ready_delay(provider),
            Duration::from_secs(12)
        );
    }
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
