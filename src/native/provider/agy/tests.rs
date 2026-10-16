use super::*;
use crate::native::terminal;
use std::path::Path;

#[test]
fn declared_transports_preserve_framing_and_pending_cleanup() {
    for facts in [AgyInitialPaste.facts(), AgyPaste.facts()] {
        let input = facts.surface.unwrap();
        assert_eq!(input.submit_count, 1);
        assert_eq!(input.newline, terminal::ConsoleNewline::LineFeed);
    }
    use crate::native::delivery::TurnKind;
    let directory = tempfile::tempdir().unwrap();
    let surface = serde_json::from_value(
        serde_json::json!({"terminal":"windows-console", "session_id":"surface"}),
    )
    .unwrap();
    let initial =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::Initial);
    install_pending_turn(directory.path(), initial.claim_token).unwrap();
    let before = std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap();
    let prepared = AgyInitialPaste.prepare(&initial).unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_initial_prompt(directory.path(), initial.prompt).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    AgyInitialPaste.discard_prepared(&initial, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
    let follow =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::FollowUp);
    let prepared = AgyPaste.prepare(&follow).unwrap();
    let pending = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_correlated_prompt(follow.prompt, &pending, cfg!(windows)).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    AgyPaste.discard_prepared(&follow, Some(&prepared));
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
    AgyPaste.prepare(&follow).unwrap();
    AgyPaste.discard_prepared(&follow, None);
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
}

#[test]
fn workspace_trust_and_dialog_are_exact_and_do_not_accept_permission_prompts() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let homes = consent::fixture_homes(tmp.path());
    let key = consent::native_key(&workspace).unwrap();
    super::super::super::write_json_atomic(
        &homes.agy,
        &serde_json::json!({"trustedWorkspaces":[key.clone()]}),
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
        "Accessing workspace:\n{key}\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\nNo, exit\n↑/↓ Navigate · enter Confirm\nGemini 3.8 Flash · high"
    );
    assert_eq!(
        agy_trust_prompt_key(&screen, &workspace),
        Some(terminal::DialogKey::Enter)
    );
    // The footer names the saved model, which need not be a Gemini one, and it
    // is absent in the first second (session-a1QguW and session-n3tsmp).
    for footer in ["Claude Opus 4.6 (Thinking)", "GPT-OSS 120B (Medium)"] {
        assert_eq!(
            agy_trust_prompt_key(
                &screen.replace("Gemini 3.8 Flash · high", footer),
                &workspace
            ),
            Some(terminal::DialogKey::Enter),
            "{footer}"
        );
    }
    assert_eq!(
        agy_trust_prompt_key(
            screen.trim_end_matches("\nGemini 3.8 Flash · high"),
            &workspace
        ),
        Some(terminal::DialogKey::Enter)
    );
    assert_eq!(
        agy_trust_prompt_key(&screen, &workspace.join("child")),
        None
    );
    assert_eq!(
        agy_trust_prompt_key(&(screen.clone() + "\nAllow terminal command?"), &workspace),
        None
    );
    assert_eq!(
        agy_trust_prompt_key(
            &screen.replace("> Yes, I trust this folder", "Yes, I trust this folder"),
            &workspace
        ),
        None
    );
    std::fs::write(&homes.agy, b"{\"trustedWorkspaces\":true}").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
}

#[test]
fn windows_console_prompt_preserves_multiline_input_without_raw_submission_keys() {
    let pending = PendingAgyTurn::new("1-2-3").unwrap();
    let prompt = "first line\nsecond\tcolumn";
    let framed = terminal_correlated_prompt(prompt, &pending, true).unwrap();

    assert!(
        framed
            .chars()
            .all(|character| !matches!(character, '\r' | '\n' | '\t'))
    );
    assert!(framed.contains(&serde_json::to_string(prompt).unwrap()));
    assert!(framed.contains(&pending.marker));
}

#[test]
fn agy_rejects_the_native_hook_transport_it_does_not_use() {
    let payload = serde_json::json!({ "last_assistant_message": "unexpected" });
    assert!(ADAPTER.handle_hook(Path::new("unused"), &payload).is_err());
}

#[test]
fn reopen_is_refused_with_the_adapters_own_reason() {
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), AGY_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Agy"),
        "{error}"
    );
    let directory = tempfile::tempdir().unwrap();
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), AGY_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Agy"),
        "{error}"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

// The expected policy prefix of an Agy command, in the fixed order the adapter emits it:
// the bypass only when requested, then effort, then model.
fn agy_policy(yolo: bool, model: Option<&str>, effort: Option<&str>) -> Vec<OsString> {
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
// policy, the private log file, and off Windows the correlated interactive prompt.
#[test]
fn launch_plan_is_the_complete_command_for_every_policy_and_platform() {
    for windows in [false, true] {
        for yolo in [false, true] {
            for model in [None, Some("gemini-model")] {
                for effort in [None, Some("high")] {
                    let root = tempfile::tempdir().unwrap();
                    let directory = root.path().join("session-safe123");
                    std::fs::create_dir(&directory).unwrap();
                    std::fs::create_dir(directory.join("events")).unwrap();
                    turn::claim(&Store::open_unchecked(&directory), &[])
                        .unwrap()
                        .retain();
                    let plan = prepare_launch_for_platform(
                        LaunchContext {
                            bridge_executable: Path::new("/opt/agent-bridge"),
                            directory: &directory,
                            workspace: root.path(),
                            title: "Agy test",
                            prompt: "agy prompt",
                            yolo,
                            model,
                            effort,
                        },
                        windows,
                    )
                    .unwrap();
                    let label =
                        format!("windows={windows} yolo={yolo} model={model:?} effort={effort:?}");
                    let pending = read_pending_turn(&directory).unwrap().unwrap();
                    assert_eq!(
                        pending.claim_token,
                        turn::current_claim_token(&Reader::open_unchecked(&directory))
                            .unwrap()
                            .unwrap(),
                        "{label}"
                    );
                    let log_path = directory.join(AGY_LOG_FILE);
                    let mut expected = agy_policy(yolo, model, effort);
                    expected.extend([
                        OsString::from("--log-file"),
                        log_path.clone().into_os_string(),
                    ]);
                    if !windows {
                        let framed = correlated_prompt("agy prompt", &pending);
                        assert!(framed.starts_with("agy prompt"), "{label}");
                        assert!(framed.ends_with(&pending.marker), "{label}");
                        expected.extend([
                            OsString::from("--prompt-interactive"),
                            OsString::from(framed),
                        ]);
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
                    assert!(
                        matches!(
                            &plan.completion_monitor,
                            CompletionMonitor::AgyTranscript { log_path: path } if *path == log_path
                        ),
                        "{label}: {:?}",
                        plan.completion_monitor
                    );
                    assert!(plan.environment_removals.is_empty(), "{label}");
                }
            }
        }
    }
}
