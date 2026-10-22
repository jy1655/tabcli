use super::*;
use crate::native::terminal;
use std::path::Path;

#[test]
fn declared_transports_preserve_framing_and_pending_cleanup() {
    for facts in [AgyInitialPaste::new().facts(), AgyPaste::new().facts()] {
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
    let prepared = AgyInitialPaste::new().prepare(&initial).unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_initial_prompt(directory.path(), initial.prompt).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    AgyInitialPaste::new().discard_prepared(&initial, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
    let follow =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::FollowUp);
    let prepared = AgyPaste::new().prepare(&follow).unwrap();
    let pending = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_correlated_prompt(follow.prompt, &pending, cfg!(windows)).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    AgyPaste::new().discard_prepared(&follow, Some(&prepared));
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
    AgyPaste::new().prepare(&follow).unwrap();
    AgyPaste::new().discard_prepared(&follow, None);
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
}

#[test]
fn workspace_trust_and_dialog_are_exact_and_do_not_accept_permission_prompts() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let env = consent::Environment::at_home(tmp.path());
    std::fs::create_dir_all(tmp.path().join(".gemini/antigravity-cli")).unwrap();
    let store = tmp.path().join(".gemini/antigravity-cli/settings.json");
    let key = consent::native_key(&workspace).unwrap();
    crate::native::session::primitives::write_json_atomic(
        &store,
        &serde_json::json!({"trustedWorkspaces":[key.clone()]}),
    )
    .unwrap();
    // The evidence names the store the adapter read, spelled as its home resolves it:
    // one join, whose slashes stay on every platform.
    let Trust::Trusted(evidence) = ADAPTER.workspace_trust(&workspace, &env, None).unwrap() else {
        panic!("trusted");
    };
    assert_eq!(evidence.provider, "agy");
    assert_eq!(evidence.key, key);
    assert_eq!(evidence.store, store);
    let serialized = serde_json::to_value(&evidence).unwrap()["store"]
        .as_str()
        .unwrap()
        .to_owned();
    if cfg!(windows) {
        assert!(
            serialized.ends_with(r"\.gemini/antigravity-cli/settings.json"),
            "{serialized}"
        );
    } else {
        assert!(
            serialized.ends_with("/.gemini/antigravity-cli/settings.json"),
            "{serialized}"
        );
    }
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &env, None)
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
    std::fs::write(&store, b"{\"trustedWorkspaces\":true}").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &env, None).is_err());
}

#[test]
fn the_agy_home_is_the_settings_file_under_the_user_home() {
    use crate::native::consent::Environment;
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    assert_eq!(
        Home::resolve(&Environment::at_home(home.path()))
            .unwrap()
            .settings,
        home.path().join(".gemini/antigravity-cli/settings.json")
    );
    let profile_only = Environment {
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&profile_only).unwrap().settings,
        home.path().join(".gemini/antigravity-cli/settings.json")
    );
    // `HOME`, even empty, before `USERPROFILE`; no home at all is the trust lookup's error.
    let empty_home = Environment {
        home: Some("".into()),
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&empty_home).unwrap().settings,
        PathBuf::from("").join(".gemini/antigravity-cli/settings.json")
    );
    assert_eq!(
        Home::resolve(&Environment::default())
            .unwrap_err()
            .to_string(),
        "no user home for workspace trust lookup"
    );
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
                    assert_eq!(
                        format!("{:?}", plan.watcher),
                        format!("{:?}", AgyTranscriptWatcher { log_path }),
                        "{label}"
                    );
                    assert!(plan.environment_removals.is_empty(), "{label}");
                }
            }
        }
    }
}

// The plan carries the transcript watcher unstarted: building it leaves the session's log
// path untouched and runs nothing; `start` hands the directory and the owned log path to
// the monitor, whose first read then fails on the non-regular log, and `stop` returns
// the monitor's own error.
#[test]
fn launch_plan_watcher_starts_the_transcript_monitor_only_when_started() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    let log_path = directory.join(AGY_LOG_FILE);
    std::fs::create_dir(&log_path).unwrap();
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: root.path(),
            title: "Agy test",
            prompt: "agy prompt",
            yolo: false,
            model: None,
            effort: None,
        },
        false,
    )
    .unwrap();
    assert_eq!(
        format!("{:?}", plan.watcher),
        format!(
            "{:?}",
            AgyTranscriptWatcher {
                log_path: log_path.clone()
            }
        )
    );
    assert!(log_path.is_dir(), "building the plan touched the log path");
    assert!(!directory.join("status.json").exists());

    let active = plan.watcher.start(&directory).unwrap();
    let error = active.stop().unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!(
            "refusing non-regular session file: {}",
            log_path.display()
        )),
        "{error:#}"
    );
}
