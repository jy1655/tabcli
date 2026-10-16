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
fn console_paste_declares_soft_newlines_and_rejects_only_console_tabs() {
    use crate::native::delivery::TurnKind;
    let facts = CodexInitialPaste.facts().surface.unwrap();
    assert_eq!(facts.submit_count, 2);
    assert_eq!(facts.newline, terminal::ConsoleNewline::ShiftReturn);
    assert!(CodexQueue.facts().surface.is_none());
    let directory = tempfile::tempdir().unwrap();
    for kind in [
        "windows-console",
        "ghostty",
        "iterm2",
        "apple-terminal",
        "wezterm",
        "warp",
    ] {
        let surface = serde_json::from_value(serde_json::json!({
            "terminal": kind, "session_id": "surface"
        }))
        .unwrap();
        let mut turn =
            super::super::transport_test_context(directory.path(), &surface, TurnKind::Initial);
        install_pending_turn(directory.path(), turn.claim_token).unwrap();
        turn.prompt = "first\n\tsecond";
        let result = CodexInitialPaste.prepare(&turn);
        if kind == "windows-console" {
            assert_eq!(
                result.err().unwrap().to_string(),
                "Codex Windows console paste cannot preserve tabs; replace tab characters with spaces"
            );
        } else {
            assert!(result.unwrap().prompt.contains(turn.prompt));
        }
        turn.kind = TurnKind::FollowUp;
        assert!(CodexQueue.prepare(&turn).is_ok());
        turn.kind = TurnKind::Initial;
        turn.prompt = "first\nsecond";
        assert!(
            CodexInitialPaste
                .prepare(&turn)
                .unwrap()
                .prompt
                .contains(turn.prompt)
        );
    }
}

#[test]
fn declared_transports_preserve_framing_and_pending_cleanup() {
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
    let prepared = CodexInitialPaste.prepare(&initial).unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_initial_prompt(directory.path(), initial.prompt).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    CodexInitialPaste.discard_prepared(&initial, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
    let follow =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::FollowUp);
    let prepared = CodexQueue.prepare(&follow).unwrap();
    assert!(prepared.prompt.is_empty());
    assert!(prepared.correlation_id.is_none());
    CodexQueue.discard_prepared(&follow, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
}

#[test]
fn workspace_trust_reads_exact_toml_key_and_no_permission_override() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let homes = consent::fixture_homes(tmp.path());
    let mut key = consent::native_key(&workspace).unwrap();
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    let config = format!(
        "[projects.{}]\ntrust_level = \"trusted\"\n",
        serde_json::to_string(&key).unwrap()
    );
    // Created as Codex creates it in a user profile: nobody else can change it. A
    // plain file in the temporary directory inherits whatever that directory allows.
    super::super::super::write_private(&homes.codex, config.as_bytes()).unwrap();
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
    std::fs::write(
        &homes.codex,
        config.replace("trusted", "untrusted").as_bytes(),
    )
    .unwrap();
    assert_eq!(
        ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
        Trust::Declined
    );
    std::fs::write(&homes.codex, b"[bad").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
}

#[test]
fn codex_native_queue_requires_version_0_149_or_newer() {
    for version in ["codex-cli 0.147.0", "codex-cli 0.148.9"] {
        assert!(!codex_version_supports_native_queue(version).unwrap());
    }
    for version in ["codex-cli 0.149.0", "codex-cli 0.153.2"] {
        assert!(codex_version_supports_native_queue(version).unwrap());
    }
    assert!(codex_version_supports_native_queue("codex unknown").is_err());
}

#[test]
fn terminal_follow_up_refuses_before_installing_pending_turn_metadata() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        ADAPTER.follow_up_transport().facts().surface.is_none(),
        "terminal ownership does not identify the active Codex thread"
    );
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
}

#[test]
fn reopen_is_refused_with_the_adapters_own_reason() {
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), CODEX_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Codex"),
        "{error}"
    );
    let directory = tempfile::tempdir().unwrap();
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), CODEX_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Codex"),
        "{error}"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

fn homes_with_codex_config(codex: &str) -> crate::native::consent::Homes {
    crate::native::consent::Homes {
        codex: std::path::PathBuf::from(codex),
        claude: "unused-claude".into(),
        agy: "unused-agy".into(),
        pi: "unused-pi".into(),
    }
}

#[test]
fn the_memory_workspace_is_beside_the_config_that_homes_names() {
    // Only the Codex config path decides; the other providers' homes are never read.
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("custom-codex").join("config.toml");
    assert_eq!(
        memory_workspace(&homes_with_codex_config(config.to_str().unwrap())).unwrap(),
        home.path().join("custom-codex").join("memories")
    );
    // A relative config path stays relative: the caller's working directory resolves it.
    assert_eq!(
        memory_workspace(&homes_with_codex_config("relative/config.toml")).unwrap(),
        std::path::Path::new("relative").join("memories")
    );
    assert_eq!(
        memory_workspace(&homes_with_codex_config("config.toml")).unwrap(),
        std::path::Path::new("memories")
    );
    let error = memory_workspace(&homes_with_codex_config("")).unwrap_err();
    assert_eq!(error.to_string(), "Codex config path has no parent");
    assert_eq!(
        super::super::codex_memory_workspace(&homes_with_codex_config(config.to_str().unwrap()))
            .unwrap(),
        memory_workspace(&homes_with_codex_config(config.to_str().unwrap())).unwrap()
    );
}

// A session directory with a seeded Claim, as the launcher leaves it before the wrapper
// asks for the plan, and the workspace the session runs in.
fn launch_fixture(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let directory = root.join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    (directory, workspace.canonicalize().unwrap())
}

// The expected policy prefix of a Codex command, in the fixed order the adapter emits it:
// the bypass only when requested, then effort, then model.
fn codex_policy(yolo: bool, model: Option<&str>, effort: Option<&str>) -> Vec<OsString> {
    let mut expected = Vec::new();
    if yolo {
        expected.push(OsString::from("--dangerously-bypass-approvals-and-sandbox"));
    }
    if let Some(effort) = effort {
        expected.extend([
            OsString::from("-c"),
            OsString::from(format!("model_reasoning_effort=\"{effort}\"")),
        ]);
    }
    if let Some(model) = model {
        expected.extend([OsString::from("--model"), OsString::from(model)]);
    }
    expected
}

// The complete launch command for every policy combination, both platform branches and
// both consent answers: policy first, then the trust override when consent authorizes it,
// then the notify hook, and off Windows the workspace and the correlated framed prompt.
#[test]
fn launch_plan_is_the_complete_command_for_every_policy_platform_and_consent() {
    let bridge = Path::new("/opt/agent-bridge");
    let notify = format!(
        "notify={}",
        serde_json::to_string(&[bridge.to_string_lossy().as_ref(), "native-hook", "codex"])
            .unwrap()
    );
    let prompt = crate::native::native_delegation_prompt("parent", "review this");
    for consent in [false, true] {
        for windows in [false, true] {
            for yolo in [false, true] {
                for (model, mapped) in [
                    (None, None),
                    (Some("openai-codex/gpt-5.6-sol"), Some("gpt-5.6-sol")),
                ] {
                    for effort in [None, Some("xhigh")] {
                        let root = tempfile::tempdir().unwrap();
                        let (directory, workspace) = launch_fixture(root.path());
                        // The authorized rows use the adapter's gate seam with a
                        // positive answer and the real applied record; the absent rows
                        // use the production gate, which has no consent record to read.
                        let authorized: fn(&Path, FirstPartyCli) -> Result<bool> = if consent {
                            crate::native::consent::prepare(&directory, &workspace).unwrap();
                            |_, _| Ok(true)
                        } else {
                            crate::native::consent::authorized
                        };
                        let plan = prepare_launch_for_platform(
                            LaunchContext {
                                bridge_executable: bridge,
                                directory: &directory,
                                workspace: &workspace,
                                title: "Codex test",
                                prompt: &prompt,
                                yolo,
                                model,
                                effort,
                            },
                            windows,
                            authorized,
                        )
                        .unwrap();
                        let label = format!(
                            "consent={consent} windows={windows} yolo={yolo} model={model:?} effort={effort:?}"
                        );
                        let pending = read_pending_turn(&directory).unwrap().unwrap();
                        assert_eq!(
                            pending.claim_token,
                            turn::current_claim_token(&Reader::open_unchecked(&directory))
                                .unwrap()
                                .unwrap(),
                            "{label}"
                        );
                        let mut expected = codex_policy(yolo, mapped, effort);
                        if consent {
                            expected.extend([
                                OsString::from("-c"),
                                OsString::from(workspace_trust_override(&workspace).unwrap()),
                            ]);
                        }
                        expected.extend([OsString::from("-c"), OsString::from(&notify)]);
                        if !windows {
                            let framed = correlated_prompt(&prompt, &pending);
                            assert!(framed.starts_with(&prompt), "{label}");
                            assert!(framed.ends_with(&pending.marker), "{label}");
                            expected.extend([
                                OsString::from("-C"),
                                workspace.as_os_str().to_owned(),
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
                        assert_no_op_watcher(plan.watcher);
                        assert!(plan.environment_removals.is_empty(), "{label}");
                        let observed = crate::native::consent::observe(&directory);
                        if consent {
                            assert_eq!(observed["applied"], APPLIED_MARKER, "{label}");
                        } else {
                            assert!(observed.is_null(), "{label}: {observed}");
                        }
                    }
                }
            }
        }
    }
}
