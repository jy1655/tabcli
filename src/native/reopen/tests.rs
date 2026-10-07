use super::*;
use crate::native::session::close::compatibility::*;
use crate::native::tests::{
    spawn_surviving_process, test_windows_process_identity, write_provider_process_record,
};

const REOPEN_TEST_CONVERSATION: &str = "6928ca1c-1234-4abc-8def-0123456789ab";

#[test]
fn creation_binds_both_sessions_or_releases_the_source_reservation() {
    for failure in [None, Some("create"), Some("provenance")] {
        let root = tempfile::tempdir().unwrap();
        let source_id = "session-creation-source";
        let directory = write_closed_reopen_source(
            root.path(),
            source_id,
            "claude",
            Some(REOPEN_TEST_CONVERSATION),
            true,
        );
        let source = inspect_reopen_source(&directory, source_id).unwrap();
        let before = snapshot_directory(&directory);
        let mut created_directory = None;
        let result = create_reopened_session(&directory, source_id, &source, || {
            // The source must already exclude another Reopen before creation starts.
            assert!(claim_reopen_marker(&directory, source_id, REOPEN_TEST_CONVERSATION).is_err());
            if failure == Some("create") {
                bail!("injected creation failure");
            }
            let created = create_session_within(
                root.path(),
                &[root.path().to_owned()],
                SessionSpec {
                    provider: FirstPartyCli::Claude,
                    provider_path: PathBuf::from("claude"),
                    provider_version: "fixture".to_owned(),
                    workspace: root.path().to_owned(),
                    title: "reopened".to_owned(),
                    model: None,
                    effort: None,
                    yolo: false,
                    prompt: "test".to_owned(),
                },
            )?;
            created_directory = Some(created.directory.clone());
            if failure == Some("provenance") {
                fs::remove_file(created.directory.join("manifest.json"))?;
                fs::create_dir(created.directory.join("manifest.json"))?;
            }
            Ok(created)
        });
        if failure.is_some() {
            assert!(result.is_err());
            assert_eq!(snapshot_directory(&directory), before);
            if let Some(created) = created_directory {
                assert_eq!(
                    Reader::open_unchecked(created).status().unwrap().state,
                    SessionState::Failed
                );
            }
        } else {
            let (created, provenance) = result.unwrap();
            assert_eq!(
                read_resumed_from(&created.directory).unwrap(),
                Some(provenance)
            );
            let marker: ReopenMarker = Reader::open_unchecked(&directory)
                .record(CoreRecord::ReopenMarker)
                .json()
                .unwrap();
            assert_eq!(marker.reopened_by.as_deref(), Some(created.id.as_str()));
            let mut after = snapshot_directory(&directory);
            after.remove(CoreRecord::ReopenMarker.name());
            assert_eq!(after, before);
            assert!(claim_reopen_marker(&directory, source_id, REOPEN_TEST_CONVERSATION).is_err());
        }
    }
}

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
        wezterm_mux: None,
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
    update_status(&directory, SessionState::Running, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    let token = claim.token().to_owned();
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

pub(in crate::native) fn snapshot_directory(
    directory: &Path,
) -> std::collections::BTreeMap<String, Vec<u8>> {
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
    update_status(&open, SessionState::Ready, None, None).unwrap();
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
    let receipt = requests::create(&Store::open_unchecked(&unresolved), "9-9-9", &[]).unwrap();
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
            .state
            .as_str(),
        "closed"
    );
}

#[test]
fn late_hook_carrying_the_previous_marker_into_the_new_directory_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("events")).unwrap();
    update_status(directory.path(), SessionState::Working, None, None).unwrap();
    let claim = acquire_turn_claim(directory.path()).unwrap();
    let token = claim.token().to_owned();
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
            .state
            .as_str(),
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
            .state
            .as_str(),
        "ready"
    );
}

#[test]
fn reopen_refuses_a_source_whose_events_path_is_not_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let source = write_closed_reopen_source(
        root.path(),
        "session-reopenevfile",
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    // The closed source is otherwise healthy; only its `events` path is damaged. The
    // shared listing would report such a path as "no events", which is not an
    // identity-less source but an unreadable one.
    let events = source.join("events");
    fs::remove_dir_all(&events).unwrap();
    fs::write(&events, b"not a directory").unwrap();
    let error = inspect_reopen_source(&source, "session-reopenevfile").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("request-unresolved"),
        "{error:#}"
    );
    let rendered = format!("{error:#}");
    assert!(rendered.contains("events is not a directory"), "{rendered}");
    assert!(!rendered.contains("source-identity-missing"), "{rendered}");

    // An absent `events` directory is not rejected by the directory gate: the source holds
    // no event, so the later gates name what is actually missing. This source recorded a
    // request, so its receipt is what no longer resolves.
    fs::remove_file(&events).unwrap();
    let error = inspect_reopen_source(&source, "session-reopenevfile").unwrap_err();
    assert_eq!(
        reopen_refusal_gate(&error),
        Some("request-unresolved"),
        "{error:#}"
    );
    let rendered = format!("{error:#}");
    assert!(rendered.contains("has no recorded result"), "{rendered}");
    assert!(!rendered.contains("cannot be read"), "{rendered}");
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
    update_status(&new, SessionState::Running, None, None).unwrap();
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

    update_status(&new, SessionState::Exited, Some(1), None).unwrap();
    update_status(
        &new,
        SessionState::Failed,
        None,
        Some("provider exited after close".to_owned()),
    )
    .unwrap();
    let status: SessionStatus = read_json(&new.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
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
            .state
            .as_str(),
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
    update_status(&directory, SessionState::Ready, None, None).unwrap();
    assert_eq!(
        query::inspect_value(&Reader::open_unchecked(&directory), id).unwrap()["resumed_from"],
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
    let value = query::inspect_value(&Reader::open_unchecked(&directory), id).unwrap();
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
    update_status(&directory, SessionState::Launching, None, None).unwrap();
    let claim = acquire_turn_claim(&directory).unwrap();
    claim.retain();

    let result = run_session_inner(&directory);
    finalize_native_session(&directory, &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    // The spawned provider is recorded with its Windows identity before the session leaves
    // its launch state; the stub has exited by now, so the record verifies it gone.
    let record: ProviderProcessRecord = read_json(&directory.join(PROVIDER_PROCESS_FILE)).unwrap();
    assert_eq!(record.schema, 1);
    assert_eq!(record.managed_session_id, id);
    assert_ne!(record.pid, 0);
    assert!(record.windows_process_identity.is_some(), "{record:?}");
    assert!(record.spawned_unix_ms > 0);
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
            created_unix_ms: Some(1),
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
            requests::list(&Reader::open_unchecked(&source))
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
                .state
                .as_str(),
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
    update_status(&directory, SessionState::Launching, None, None).unwrap();
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
    assert!(
        !directory.join(PROVIDER_PROCESS_FILE).exists(),
        "a provider process was recorded"
    );
    assert!(directory.join("initial-prompt.txt").is_file());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "failed");
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
        update_status(&new, state.parse().unwrap(), None, None).unwrap();
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
        &SessionState::Ready,
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
    assert_eq!(status.state.as_str(), "ready");
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
    update_status(&new, SessionState::Closed, None, Some(tell_reason)).unwrap();
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
    // launch refusal it looks like: the gate is named. The closed session alone does not
    // release the marker; the provider process the launch recorded must be verified gone.
    let recorded = record_reopen_refusal(
        &new,
        REOPEN_CONFLICT_GATE,
        "held by another live process; no prompt was delivered".to_owned(),
    );
    assert_eq!(reopen_refusal_gate(&recorded), Some("reopen-conflict"));
    let (outcome, gate) = settle_reopen_outcome(resolve, source_id, new_id, Err(uncertain()));
    assert_eq!(gate.as_deref(), Some("reopen-conflict"));
    let text = format!("{:#}", outcome.unwrap_err());
    assert!(
        text.starts_with(&format!(
            "the reopen marker of source session {source_id} was not released: the refused launch may still hold the conversation: refused session {new_id} is closed with no provider process record"
        )) && text.ends_with(&format!(": {:#}", uncertain())),
        "{text}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    write_provider_process_record(&new, new_id, 0);
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
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (mut refused, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let refused_request = refused.receipt().request_id.clone();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");

    // The refused claim is released out from under the refusal before it is recorded, and
    // the next `tell` claims the turn and starts working.
    release_turn_claim(directory).unwrap();
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (next, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let next_token = next.token().to_owned();
    let next_request = next.receipt().request_id.clone();
    update_status(directory, SessionState::Working, None, None).unwrap();

    let reported =
        record_follow_up_refusal("session-test", &mut refused, &SessionState::Ready, refusal);
    drop(refused);
    assert_eq!(
        format!("{reported:#}"),
        "the turn claim of session session-test already belonged to another request; its status was left unchanged: reopen refused (reopen-conflict): held by pid 4242"
    );

    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working", "{status:?}");
    assert_eq!(status.error, None, "{status:?}");
    assert_eq!(
        fs::read_to_string(directory.join(TURN_CLAIM_FILE))
            .unwrap()
            .trim(),
        next_token
    );
    let receipts = requests::list(&Reader::open_unchecked(directory)).unwrap();
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
    update_status(directory, SessionState::Ready, None, None).unwrap();
    let (mut refused, _) = acquire_ready_turn_claim(directory, "session-test").unwrap();
    let refusal = anyhow::anyhow!("reopen refused (reopen-conflict): held by pid 4242");
    let reported =
        record_follow_up_refusal("session-test", &mut refused, &SessionState::Ready, refusal);
    drop(refused);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "ready", "{status:?}");
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
        update_status(&directory, state.parse().unwrap(), None, None).unwrap();
    }
    let (mut claim, _) = acquire_ready_turn_claim_with_context(&directory, id, &[]).unwrap();
    let refused_request = claim.receipt().request_id.clone();
    assert_eq!(
        read_json::<SessionStatus>(&directory.join("status.json"))
            .unwrap()
            .state
            .as_str(),
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
        &SessionState::Ready,
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
    assert_eq!(status.state.as_str(), "ready");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(&foreign.id().to_string()),
        "{status:?}"
    );
    let result =
        query::request_result(&Reader::open_unchecked(&directory), &refused_request).unwrap();
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
        &SessionState::Ready,
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
        &SessionState::Ready,
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
    assert_eq!(status.state.as_str(), "ready");
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
// surface is closed, and the source's marker is released once the provider process the
// launch recorded is verified gone. A closed surface with a provider that survived it, and
// a refusal with no provider record at all, both keep the marker consumed.
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

    // `provider` is the pid the launch wrapper recorded for the spawned provider: 0 is a
    // process verified dead, this process stands in for one that survived, and `None` is a
    // launch that recorded no process.
    for (label, registry, expected_detail, close_succeeds, provider) in [
        (
            "unreadable registry, surface closed, provider gone",
            &unreadable_registry,
            "failed to read the Claude session registry",
            true,
            Some(0),
        ),
        (
            "registration timeout, surface closed, provider survived",
            &empty_registry,
            "did not register",
            true,
            Some(std::process::id()),
        ),
        (
            "unreadable registry, close failed, no provider record",
            &unreadable_registry,
            "failed to read the Claude session registry",
            false,
            None,
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
        update_status(&new, SessionState::Launching, None, None).unwrap();
        let mut initial_claim = acquire_turn_claim(&new).unwrap();
        let request_id = initial_claim.receipt().request_id.clone();
        update_status(&new, SessionState::AwaitingInitialInput, None, None).unwrap();

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
        initial_claim
            .settle_delivery(turn::Delivery::NotSent(&error))
            .unwrap();
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
            assert_eq!(status.state.as_str(), "closed", "{label}");
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
            assert_eq!(status.state.as_str(), "failed", "{label}");
            assert!(new.join(TERMINAL_HANDLE_FILE).is_file(), "{label}");
        }
        assert!(
            !new.join(TURN_CLAIM_FILE).exists(),
            "{label}: claim not released"
        );
        // The receipt survives with its outcome unresolved: nothing was delivered.
        let result = query::request_result(&Reader::open_unchecked(&new), &request_id).unwrap();
        assert_eq!(result["request_state"], "unresolved", "{label}: {result}");
        assert_eq!(result["result"], serde_json::Value::Null, "{label}");

        // The source is untouched except for its marker. Only a refusal whose recorded
        // provider process is verified gone releases it. A closed surface is not that
        // evidence while the provider survives it, and a refusal with no provider record
        // keeps the marker consumed: the spawned process may survive without ever
        // registering, so the next reopen's registry scan would not see it. The retention
        // is noted in the refusal record.
        let mut source_after = snapshot_directory(&source);
        assert!(
            source_after.remove("reopen.marker.json").is_some(),
            "{label}: marker missing before release"
        );
        let mut expected = source_before.clone();
        expected.remove("reopen.marker.json");
        assert_eq!(source_after, expected, "{label}");
        match provider {
            Some(pid) => write_provider_process_record(&new, new_id, pid),
            None => remove_file_if_present(&new.join(PROVIDER_PROCESS_FILE)).unwrap(),
        }
        let released = provider == Some(0);
        let retained_condition = match provider {
            None => format!(
                "refused session {new_id} is {} with no provider process record",
                status.state
            ),
            Some(pid) => {
                format!("provider process {pid} of refused session {new_id} is still running")
            }
        };
        let outcome = release_reopen_marker_after_refusal(
            &source,
            &new,
            REOPEN_VERIFICATION_FAILED_GATE,
            Err(anyhow::anyhow!("refused")),
        );
        let outcome = outcome.unwrap_err();
        if released {
            assert_eq!(format!("{outcome:#}"), "refused", "{label}");
            assert!(!source.join(REOPEN_MARKER_FILE).exists(), "{label}");
            assert_eq!(snapshot_directory(&source), expected, "{label}");
            let record: RecordedReopenRefusal = read_json(&new.join(REOPEN_REFUSAL_FILE)).unwrap();
            assert_eq!(record.cleanup, None, "{label}");
        } else {
            let text = format!("{outcome:#}");
            assert!(
                text.starts_with(&format!(
                    "the reopen marker of source session {source_id} was not released: the refused launch may still hold the conversation: {retained_condition}"
                )) && text.ends_with(": refused"),
                "{label}: {text}"
            );
            assert!(source.join(REOPEN_MARKER_FILE).is_file(), "{label}");
            let record: RecordedReopenRefusal = read_json(&new.join(REOPEN_REFUSAL_FILE)).unwrap();
            assert_eq!(record.gate, "reopen-verification-failed", "{label}");
            assert_eq!(record.cleanup.as_deref(), Some("pending"), "{label}");
            assert!(
                record
                    .cleanup_detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&retained_condition),
                "{label}: {record:?}"
            );
            // The launch wrapper is dead; that is never evidence about the provider process
            // it spawned.
            write_json_atomic(
                &new.join(SESSION_OWNER_FILE),
                &NativeSessionOwner {
                    pid: 0,
                    managed_session_id: Some(new_id.to_owned()),
                    ..NativeSessionOwner::default()
                },
            )
            .unwrap();
            // Every later reopen is refused naming the blocking condition, under both the
            // read-only gate and the claim lock, and the marker is left as it is.
            let marker_before = fs::read(source.join(REOPEN_MARKER_FILE)).unwrap();
            for record in [
                None,
                Some((std::process::id(), new_id)),
                Some((0, "session-reopenother")),
            ] {
                match record {
                    Some((pid, session)) => write_provider_process_record(&new, session, pid),
                    None => remove_file_if_present(&new.join(PROVIDER_PROCESS_FILE)).unwrap(),
                }
                let expected_condition = match record {
                    None => "with no provider process record".to_owned(),
                    Some((0, _)) => format!(
                        "the provider process record of refused session {new_id} names \"session-reopenother\""
                    ),
                    Some((pid, _)) => format!(
                        "provider process {pid} of refused session {new_id} is still running"
                    ),
                };
                for refused in [
                    inspect_reopen_source(&source, source_id)
                        .expect_err("gate admitted a retained marker"),
                    claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION)
                        .expect_err("claim admitted a retained marker"),
                ] {
                    assert_eq!(
                        reopen_refusal_gate(&refused),
                        Some("already-reopened"),
                        "{label}: {record:?}"
                    );
                    let text = format!("{refused:#}");
                    assert!(
                        text.starts_with(&format!(
                            "reopen refused (already-reopened): session {source_id} was already reopened as {new_id}; that launch was refused (reopen-verification-failed) but the refused launch may still hold the conversation: "
                        )) && text.contains(&expected_condition),
                        "{label}: {record:?}: {text}"
                    );
                }
                assert_eq!(
                    fs::read(source.join(REOPEN_MARKER_FILE)).unwrap(),
                    marker_before,
                    "{label}: {record:?}"
                );
            }
            // The provider process is later found dead: the next reopen reconciles the
            // marker below.
            write_provider_process_record(&new, new_id, 0);
        }
        assert_eq!(
            read_resumed_from(&new).unwrap().as_ref(),
            Some(&resumed_from),
            "{label}: provenance of the refused session is kept"
        );
        // A subsequent reopen of the same source passes its gates and claims the marker; a
        // retained marker whose process is now verified dead is released by that claim.
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
        assert_eq!(snapshot_directory(&source), expected, "{label}");
        fs::remove_dir_all(&new).unwrap();
    }

    // Release is refused while the refused session still accepts prompts, when a spawned
    // process cannot be verified gone, and when the marker names another reopen.
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
    update_status(&new, SessionState::Ready, None, None).unwrap();
    let outcome =
        release_reopen_marker_after_refusal(&source, &new, REOPEN_LAUNCH_GATE, Ok(())).unwrap_err();
    assert!(format!("{outcome:#}").contains("is ready"), "{outcome:#}");
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    update_status(&new, SessionState::Failed, None, Some("refused".to_owned())).unwrap();
    // A post-spawn gate with no verified cleanup retains the marker; the refusal record is
    // absent here (its write failed when the refusal was decided), so only the retention
    // itself is observable.
    let outcome = release_reopen_marker_after_refusal(&source, &new, REOPEN_CONFLICT_GATE, Ok(()))
        .unwrap_err();
    assert!(
        format!("{outcome:#}").contains("may still hold the conversation"),
        "{outcome:#}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    assert!(!new.join(REOPEN_REFUSAL_FILE).exists());
    let other = root.path().join("session-reopennew12");
    fs::create_dir(&other).unwrap();
    update_status(
        &other,
        SessionState::Failed,
        None,
        Some("refused".to_owned()),
    )
    .unwrap();
    let outcome = release_reopen_marker_after_refusal(&source, &other, REOPEN_LAUNCH_GATE, Ok(()))
        .unwrap_err();
    assert!(
        format!("{outcome:#}").contains("not the refused session"),
        "{outcome:#}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    // The pre-spawn gate is not taken at its word: a recorded provider process that is
    // still running retains the marker whatever gate the refusal names.
    write_provider_process_record(&new, new_id, std::process::id());
    let outcome =
        release_reopen_marker_after_refusal(&source, &new, REOPEN_LAUNCH_GATE, Ok(())).unwrap_err();
    assert!(
        format!("{outcome:#}").contains(&format!(
            "provider process {} of refused session {new_id} is still running",
            std::process::id()
        )),
        "{outcome:#}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
    fs::remove_file(new.join(PROVIDER_PROCESS_FILE)).unwrap();
    // The pre-spawn gate recorded no process: nothing can hold the conversation.
    release_reopen_marker_after_refusal(&source, &new, REOPEN_LAUNCH_GATE, Ok(())).unwrap();
    assert!(!source.join(REOPEN_MARKER_FILE).exists());
    // Releasing an already released marker is not an error.
    release_reopen_marker_after_refusal(&source, &new, REOPEN_LAUNCH_GATE, Ok(())).unwrap();
    provider::override_claude_session_registry_for_test(None);
}

// A marker whose parent reopen never settled is reconciled by the next reopen, under the
// source lock and from the refused session's durable records alone. A parent that crashed
// after a post-launch refusal was recorded, and a parent that timed out before the wrapper
// recorded its pre-spawn refusal, both leave the marker consumed. Nothing is inferred from
// missing records: a launch refusal must exist, and it releases the marker only once the
// refused launch is verified unable to hold the conversation.
#[test]
fn stale_launch_refusals_are_reconciled_by_the_next_reopen_only_once_cleanup_is_verified() {
    let root = tempfile::tempdir().unwrap();
    let source_id = "session-reopensrc14";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let source_before = snapshot_directory(&source);
    let expect_refused = |expected: &str| {
        let marker_before = fs::read(source.join(REOPEN_MARKER_FILE)).unwrap();
        for (label, refused) in [
            ("gate", inspect_reopen_source(&source, source_id).err()),
            (
                "claim",
                claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).err(),
            ),
        ] {
            let refused =
                refused.unwrap_or_else(|| panic!("the {label} admitted the marker: {expected}"));
            assert_eq!(
                reopen_refusal_gate(&refused),
                Some("already-reopened"),
                "{label}: {refused:#}"
            );
            assert_eq!(
                format!("{refused:#}"),
                format!("reopen refused (already-reopened): {expected}"),
                "{label}"
            );
        }
        assert_eq!(
            fs::read(source.join(REOPEN_MARKER_FILE)).unwrap(),
            marker_before,
            "a refusal altered the marker"
        );
    };
    let reconcile = || {
        // The read-only gate admits the source; the claim releases the stale marker under
        // the lock and writes its own in its place.
        inspect_reopen_source(&source, source_id).unwrap();
        let stale: ReopenMarker = read_json(&source.join(REOPEN_MARKER_FILE)).unwrap();
        let next = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
        let marker: ReopenMarker = read_json(&source.join(REOPEN_MARKER_FILE)).unwrap();
        assert_eq!(marker.reopened_by, None);
        assert_ne!(marker.claim, stale.claim);
        drop(next);
        assert_eq!(snapshot_directory(&source), source_before);
    };
    let owner = |pid: u32, session: &str| NativeSessionOwner {
        pid,
        managed_session_id: Some(session.to_owned()),
        ..NativeSessionOwner::default()
    };

    // The parent crashed after the post-launch holder check recorded its refusal and before
    // it closed the new surface or settled the marker.
    let new_id = "session-reopennew14";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    write_json_atomic(
        &new.join(TERMINAL_HANDLE_FILE),
        &reopen_test_terminal(new_id),
    )
    .unwrap();
    update_status(&new, SessionState::AwaitingInitialInput, None, None).unwrap();
    let recorded = record_reopen_refusal(
        &new,
        REOPEN_CONFLICT_GATE,
        "held by pid 4242; no prompt was delivered".to_owned(),
    );
    assert_eq!(reopen_refusal_gate(&recorded), Some("reopen-conflict"));
    expect_refused(&format!(
        "session {source_id} was already reopened as {new_id}; that launch was refused (reopen-conflict) but the refused launch may still hold the conversation: refused session {new_id} is awaiting-initial-input with no provider process record (provider-process.json), so the provider process it spawned cannot be verified gone"
    ));
    // Its wrapper is dead, but the provider process the wrapper spawned (stood in for by
    // this process) is still running: the wrapper's death is not evidence.
    write_json_atomic(&new.join(SESSION_OWNER_FILE), &owner(0, new_id)).unwrap();
    write_provider_process_record(&new, new_id, std::process::id());
    let still_running = format!(
        "session {source_id} was already reopened as {new_id}; that launch was refused (reopen-conflict) but the refused launch may still hold the conversation: provider process {} of refused session {new_id} is still running",
        std::process::id()
    );
    expect_refused(&still_running);
    // The cleanup the crashed parent never performed: an explicit close of the refused
    // session consumes its handle. The closed surface is not evidence either while the
    // provider process survives it.
    let mut closed_terminals = Vec::new();
    close_session_state_with_error(&new, Some("refused".to_owned()), |session| {
        closed_terminals.push(session.id.clone());
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(closed_terminals, [format!("{new_id}-terminal")]);
    assert!(new.join(TERMINAL_TOMBSTONE_FILE).is_file());
    expect_refused(&still_running);
    // Only the provider process being verified gone releases the marker; the closed
    // surface is reported alongside.
    write_provider_process_record(&new, new_id, 0);
    assert_eq!(
        refused_launch_cleanup(&new, REOPEN_CONFLICT_GATE)
            .unwrap()
            .to_string(),
        "provider process 0 is verified gone (it has exited) and the refused session's surface was closed"
    );
    reconcile();

    // The parent timed out while the session was still launching, before the wrapper
    // recorded anything. The marker stays consumed until a launch refusal is observable,
    // whatever else happens to the session.
    let new_id = "session-reopennew16";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    let plain = format!("session {source_id} was already reopened as {new_id}");
    expect_refused(&plain);
    write_json_atomic(&new.join(SESSION_OWNER_FILE), &owner(0, new_id)).unwrap();
    update_status(
        &new,
        SessionState::Failed,
        None,
        Some("timed out".to_owned()),
    )
    .unwrap();
    expect_refused(&plain);
    // A refusal of another phase is not a launch refusal.
    write_json_atomic(
        &new.join(REOPEN_REFUSAL_FILE),
        &serde_json::json!({
            "schema": 2,
            "phase": "follow-up",
            "gate": "reopen-conflict",
            "detail": "held by pid 4242",
            "created_unix_ms": 1,
        }),
    )
    .unwrap();
    expect_refused(&plain);
    // The wrapper's pre-spawn refusal arrives late: no process was ever spawned.
    fs::remove_file(new.join(REOPEN_REFUSAL_FILE)).unwrap();
    let recorded = record_reopen_refusal(
        &new,
        REOPEN_LAUNCH_GATE,
        "a holder registered after the initial scan".to_owned(),
    );
    assert_eq!(reopen_refusal_gate(&recorded), Some("provider-unsupported"));
    reconcile();

    // A post-launch refusal whose parent died before the close: the marker is retained
    // while the wrapper may be alive or belongs to another session, and released once its
    // process is verified gone.
    let new_id = "session-reopennew17";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    update_status(&new, SessionState::AwaitingInitialInput, None, None).unwrap();
    record_reopen_refusal(
        &new,
        REOPEN_VERIFICATION_FAILED_GATE,
        "could not verify; no prompt was delivered".to_owned(),
    );
    let prefix = format!(
        "session {source_id} was already reopened as {new_id}; that launch was refused (reopen-verification-failed) but the refused launch may still hold the conversation: "
    );
    write_json_atomic(&new.join(SESSION_OWNER_FILE), &owner(0, new_id)).unwrap();
    write_provider_process_record(&new, new_id, std::process::id());
    expect_refused(&format!(
        "{prefix}provider process {} of refused session {new_id} is still running",
        std::process::id()
    ));
    write_provider_process_record(&new, "session-reopenother", 0);
    expect_refused(&format!(
        "{prefix}the provider process record of refused session {new_id} names \"session-reopenother\" (schema 1)"
    ));
    // A record that cannot be read leaves the launch unverifiable rather than gone.
    fs::write(new.join(PROVIDER_PROCESS_FILE), "not json").unwrap();
    let unverifiable = refused_launch_cleanup(&new, REOPEN_VERIFICATION_FAILED_GATE).unwrap_err();
    assert!(
        format!("{unverifiable:#}").contains("invalid JSON"),
        "{unverifiable:#}"
    );
    assert_eq!(
        reopen_refusal_gate(&inspect_reopen_source(&source, source_id).unwrap_err()),
        Some("already-reopened")
    );
    write_provider_process_record(&new, new_id, 0);
    reconcile();

    // Markers that cannot be resolved to a refused launch are consumed, never released.
    let marker = |reopened_by: &str| ReopenMarker {
        schema: 1,
        claim: "1-2-3".to_owned(),
        provider_session_id: REOPEN_TEST_CONVERSATION.to_owned(),
        reopened_by: Some(reopened_by.to_owned()),
        created_unix_ms: 1,
    };
    write_json_atomic(
        &source.join(REOPEN_MARKER_FILE),
        &marker("session-reopenmissing"),
    )
    .unwrap();
    expect_refused(&format!(
        "session {source_id} was already reopened as session-reopenmissing; the records of session-reopenmissing are missing, so the marker is treated as consumed"
    ));
    write_json_atomic(
        &source.join(REOPEN_MARKER_FILE),
        &marker("../session-reopennew17"),
    )
    .unwrap();
    expect_refused(&format!(
        "session {source_id} was already reopened as ../session-reopennew17; the marker names an invalid session id, so it is treated as consumed"
    ));
    fs::write(source.join(REOPEN_MARKER_FILE), "not json").unwrap();
    expect_refused(&format!(
        "session {source_id} carries a reopen marker that cannot be read; it is treated as consumed"
    ));
    fs::remove_file(source.join(REOPEN_MARKER_FILE)).unwrap();
    assert_eq!(snapshot_directory(&source), source_before);
}

// The launch wrapper records the provider process it spawned, and the refusal predicate
// judges that process: a live one retains the marker under every gate, and the same record
// verifies it gone once it has exited.
#[test]
fn recorded_provider_process_is_verified_from_the_record_until_it_exits() {
    let root = tempfile::tempdir().unwrap();
    let id = "session-reopenrecord";
    let directory = write_reopen_launch_session(root.path(), id, Path::new("/opt/claude"));
    let mut child = spawn_surviving_process();
    record_provider_process(&directory, id, &child).unwrap();
    let record: ProviderProcessRecord = read_json(&directory.join(PROVIDER_PROCESS_FILE)).unwrap();
    assert_eq!(record.schema, 1);
    assert_eq!(record.managed_session_id, id);
    assert_eq!(record.pid, child.id());
    assert_eq!(
        record.windows_process_identity,
        test_windows_process_identity(child.id())
    );
    assert!(record.spawned_unix_ms > 0);
    assert_eq!(
        observe_provider_process(&record),
        ProviderProcessObservation::Alive
    );

    update_status(&directory, SessionState::AwaitingInitialInput, None, None).unwrap();
    record_reopen_refusal(
        &directory,
        REOPEN_CONFLICT_GATE,
        "held by pid 4242; no prompt was delivered".to_owned(),
    );
    for gate in REOPEN_POST_CREATION_GATES {
        assert_eq!(
            refused_launch_cleanup(&directory, gate).unwrap(),
            RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {id} is still running",
                child.id()
            )),
            "{gate}"
        );
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        observe_provider_process(&record),
        ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
    );
    for gate in REOPEN_POST_CREATION_GATES {
        let cleanup = refused_launch_cleanup(&directory, gate).unwrap();
        assert_eq!(
            cleanup,
            RefusedLaunchCleanup::ProviderProcessGone {
                pid: record.pid,
                evidence: ProviderProcessGone::Exited,
                surface_closed: false,
            },
            "{gate}"
        );
        assert!(cleanup.releases_marker());
        assert_eq!(
            cleanup.to_string(),
            format!(
                "provider process {} is verified gone (it has exited)",
                record.pid
            )
        );
    }
}

// The case the marker exists for: the launch wrapper died and the refused session's console
// was closed, but the provider process survived both (Windows does not end a child with its
// parent). The marker is retained until that process is verified gone, and the next reopen
// then releases it.
#[test]
fn surviving_provider_process_retains_the_source_marker_until_it_is_verified_gone() {
    let root = tempfile::tempdir().unwrap();
    let source_id = "session-reopensrc18";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let source_before = snapshot_directory(&source);
    let new_id = "session-reopennew18";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    write_json_atomic(
        &new.join(TERMINAL_HANDLE_FILE),
        &reopen_test_terminal(new_id),
    )
    .unwrap();
    // The wrapper recorded its owner record and the provider it spawned, then the
    // post-launch check refused.
    let mut provider = spawn_surviving_process();
    record_provider_process(&new, new_id, &provider).unwrap();
    update_status(&new, SessionState::AwaitingInitialInput, None, None).unwrap();
    let refusal = record_reopen_refusal(
        &new,
        REOPEN_CONFLICT_GATE,
        "held by pid 4242; no prompt was delivered".to_owned(),
    );
    // The parent closed the surface (the console is gone) and the wrapper died with it;
    // the provider ignored the console close.
    let mut closed_terminals = Vec::new();
    close_session_state_with_error(&new, Some(format!("{refusal:#}")), |session| {
        closed_terminals.push(session.id.clone());
        Ok(terminal::CloseOutcome::Closed)
    })
    .unwrap();
    assert_eq!(closed_terminals, [format!("{new_id}-terminal")]);
    write_json_atomic(
        &new.join(SESSION_OWNER_FILE),
        &NativeSessionOwner {
            pid: 0,
            managed_session_id: Some(new_id.to_owned()),
            ..NativeSessionOwner::default()
        },
    )
    .unwrap();
    let status: SessionStatus = read_json(&new.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "closed");
    assert!(new.join(TERMINAL_TOMBSTONE_FILE).is_file());
    assert!(query::observe_owner(&Reader::open_unchecked(&new)).process_alive == Some(false));

    let condition = format!(
        "provider process {} of refused session {new_id} is still running",
        provider.id()
    );
    let retained = format!("the refused launch may still hold the conversation: {condition}");
    // Settlement by the parent retains the marker and notes why.
    let outcome = release_reopen_marker_after_refusal(&source, &new, REOPEN_CONFLICT_GATE, Ok(()))
        .unwrap_err();
    assert!(format!("{outcome:#}").ends_with(&retained), "{outcome:#}");
    let record: RecordedReopenRefusal = read_json(&new.join(REOPEN_REFUSAL_FILE)).unwrap();
    assert_eq!(record.cleanup.as_deref(), Some("pending"));
    assert_eq!(record.cleanup_detail.as_deref(), Some(condition.as_str()));
    // Every later reopen is refused under the gate and under the lock.
    let marker_before = fs::read(source.join(REOPEN_MARKER_FILE)).unwrap();
    for refused in [
        inspect_reopen_source(&source, source_id).unwrap_err(),
        claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap_err(),
    ] {
        assert_eq!(reopen_refusal_gate(&refused), Some("already-reopened"));
        assert_eq!(
            format!("{refused:#}"),
            format!(
                "reopen refused (already-reopened): session {source_id} was already reopened as {new_id}; that launch was refused (reopen-conflict) but {retained}"
            )
        );
    }
    assert_eq!(
        fs::read(source.join(REOPEN_MARKER_FILE)).unwrap(),
        marker_before
    );

    // The provider exits. The next reopen verifies it gone and releases the marker under
    // the source lock; the closed surface is reported with it.
    provider.kill().unwrap();
    provider.wait().unwrap();
    assert_eq!(
        refused_launch_cleanup(&new, REOPEN_CONFLICT_GATE)
            .unwrap()
            .to_string(),
        format!(
            "provider process {} is verified gone (it has exited) and the refused session's surface was closed",
            provider.id()
        )
    );
    inspect_reopen_source(&source, source_id).unwrap();
    let next = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    let marker: ReopenMarker = read_json(&source.join(REOPEN_MARKER_FILE)).unwrap();
    assert_eq!(marker.reopened_by, None);
    drop(next);
    assert_eq!(snapshot_directory(&source), source_before);
}

// The reopen-local observation keeps a confirmed identity mismatch (a reused pid, which
// proves the recorded process gone) apart from a process that cannot be inspected (which
// proves nothing and retains the marker), and from a pid that is simply dead.
#[test]
fn provider_process_identity_check_separates_reuse_from_uninspectable() {
    let alive = std::process::id();
    assert_eq!(
        classify_provider_process_identity(
            alive,
            Ok(terminal::WindowsProcessIdentityCheck::Matches)
        ),
        ProviderProcessObservation::Alive
    );
    assert_eq!(
        classify_provider_process_identity(
            alive,
            Ok(terminal::WindowsProcessIdentityCheck::Mismatch(
                "Windows console process id was reused"
            ))
        ),
        ProviderProcessObservation::Gone(ProviderProcessGone::IdentityMismatch(
            "Windows console process id was reused"
        ))
    );
    assert_eq!(
        classify_provider_process_identity(alive, Err(anyhow::anyhow!("access is denied"))),
        ProviderProcessObservation::Unknown("access is denied".to_owned())
    );
    assert_eq!(
        classify_provider_process_identity(0, Err(anyhow::anyhow!("no such process"))),
        ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
    );
}

// A recorded provider pid that is alive under a different creation time or executable is a
// reused pid: the recorded process is gone and the marker is released. The same pid with
// its recorded identity is the surviving provider and retains it.
#[cfg(windows)]
#[test]
fn reused_provider_pid_with_a_different_identity_releases_the_marker() {
    let root = tempfile::tempdir().unwrap();
    let source_id = "session-reopensrc19";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let source_before = snapshot_directory(&source);
    let new_id = "session-reopennew19";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    update_status(&new, SessionState::Failed, None, Some("refused".to_owned())).unwrap();
    record_reopen_refusal(
        &new,
        REOPEN_VERIFICATION_FAILED_GATE,
        "could not verify; no prompt was delivered".to_owned(),
    );
    let pid = std::process::id();
    let identity = terminal::windows_process_identity(pid).unwrap();
    let record = |identity: terminal::WindowsProcessIdentity| {
        write_json_atomic(
            &new.join(PROVIDER_PROCESS_FILE),
            &ProviderProcessRecord {
                schema: 1,
                managed_session_id: new_id.to_owned(),
                pid,
                windows_process_identity: Some(identity),
                spawned_unix_ms: 1,
            },
        )
        .unwrap();
    };
    let already = format!(
        "reopen refused (already-reopened): session {source_id} was already reopened as {new_id}; that launch was refused (reopen-verification-failed) but "
    );

    record(identity.clone());
    assert_eq!(
        refused_launch_cleanup(&new, REOPEN_VERIFICATION_FAILED_GATE).unwrap(),
        RefusedLaunchCleanup::Pending(format!(
            "provider process {pid} of refused session {new_id} is still running"
        ))
    );
    assert_eq!(
        format!(
            "{:#}",
            inspect_reopen_source(&source, source_id).unwrap_err()
        ),
        format!(
            "{already}the refused launch may still hold the conversation: provider process {pid} of refused session {new_id} is still running"
        )
    );

    for (label, reused, reason) in [
        (
            "creation time",
            terminal::WindowsProcessIdentity {
                creation_time: identity.creation_time.wrapping_add(1),
                ..identity.clone()
            },
            "Windows console process id was reused",
        ),
        (
            "executable path",
            terminal::WindowsProcessIdentity {
                executable_path: format!("{}.other", identity.executable_path),
                ..identity.clone()
            },
            "Windows console process executable identity changed",
        ),
    ] {
        record(reused);
        let cleanup = refused_launch_cleanup(&new, REOPEN_VERIFICATION_FAILED_GATE).unwrap();
        assert_eq!(
            cleanup,
            RefusedLaunchCleanup::ProviderProcessGone {
                pid,
                evidence: ProviderProcessGone::IdentityMismatch(reason),
                surface_closed: false,
            },
            "{label}"
        );
        assert_eq!(
            cleanup.to_string(),
            format!(
                "provider process {pid} is verified gone (the pid now belongs to another process: {reason})"
            ),
            "{label}"
        );
        inspect_reopen_source(&source, source_id).unwrap();
    }
    let next = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    drop(next);
    assert_eq!(snapshot_directory(&source), source_before);
}

// A recorded provider process whose identity cannot be inspected while its pid is alive is
// neither verified surviving nor verified gone, and the marker stays consumed. The System
// process (pid 4) is alive and cannot report an executable image.
#[cfg(windows)]
#[test]
fn uninspectable_provider_process_retains_the_marker() {
    let root = tempfile::tempdir().unwrap();
    let source_id = "session-reopensrc20";
    let source = write_closed_reopen_source(
        root.path(),
        source_id,
        "claude",
        Some(REOPEN_TEST_CONVERSATION),
        true,
    );
    let new_id = "session-reopennew20";
    let new = write_reopen_launch_session(root.path(), new_id, Path::new("/opt/claude"));
    let claim = claim_reopen_marker(&source, source_id, REOPEN_TEST_CONVERSATION).unwrap();
    claim.finalize(new_id).unwrap();
    update_status(&new, SessionState::Failed, None, Some("refused".to_owned())).unwrap();
    record_reopen_refusal(
        &new,
        REOPEN_CONFLICT_GATE,
        "held by pid 4242; no prompt was delivered".to_owned(),
    );
    let system_pid = 4;
    assert!(process_is_alive(system_pid));
    let error = terminal::check_windows_process_identity(
        system_pid,
        &terminal::WindowsProcessIdentity {
            creation_time: 1,
            executable_path: "C:\\Windows\\System32\\claude.exe".to_owned(),
        },
    )
    .expect_err("the System process reported an identity");
    write_json_atomic(
        &new.join(PROVIDER_PROCESS_FILE),
        &ProviderProcessRecord {
            schema: 1,
            managed_session_id: new_id.to_owned(),
            pid: system_pid,
            windows_process_identity: Some(terminal::WindowsProcessIdentity {
                creation_time: 1,
                executable_path: "C:\\Windows\\System32\\claude.exe".to_owned(),
            }),
            spawned_unix_ms: 1,
        },
    )
    .unwrap();
    let cleanup = refused_launch_cleanup(&new, REOPEN_CONFLICT_GATE).unwrap();
    assert_eq!(
        cleanup,
        RefusedLaunchCleanup::Pending(format!(
            "provider process {system_pid} of refused session {new_id} could not be verified: {error:#}"
        ))
    );
    assert!(!cleanup.releases_marker());
    let refused = inspect_reopen_source(&source, source_id).unwrap_err();
    assert_eq!(reopen_refusal_gate(&refused), Some("already-reopened"));
    assert!(
        format!("{refused:#}").contains("could not be verified"),
        "{refused:#}"
    );
    assert!(source.join(REOPEN_MARKER_FILE).is_file());
}
