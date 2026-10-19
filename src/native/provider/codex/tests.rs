use super::*;

// The session watcher waits without a Claim or binding. Starting and stopping on an
// absent session directory must leave it absent and publish no observation records.
fn assert_idle_watcher(watcher: Box<dyn crate::native::provider::CompletionWatcher>) {
    assert_eq!(format!("{watcher:?}"), "Watcher");
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
    let env = consent::Environment::at_home(tmp.path());
    std::fs::create_dir(tmp.path().join(".codex")).unwrap();
    let store = tmp.path().join(".codex").join("config.toml");
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
    crate::native::session::primitives::write_private(&store, config.as_bytes()).unwrap();
    // The evidence names the store the adapter read, spelled as its home resolves it.
    let Trust::Trusted(evidence) = ADAPTER.workspace_trust(&workspace, &env, None).unwrap() else {
        panic!("trusted");
    };
    assert_eq!(evidence.provider, "codex");
    assert_eq!(evidence.key, key);
    assert_eq!(evidence.store, store);
    let serialized = serde_json::to_value(&evidence).unwrap()["store"]
        .as_str()
        .unwrap()
        .to_owned();
    if cfg!(windows) {
        assert!(serialized.ends_with(r"\.codex\config.toml"), "{serialized}");
    } else {
        assert!(serialized.ends_with("/.codex/config.toml"), "{serialized}");
    }
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &env, None)
            .unwrap(),
        Trust::Absent
    );
    std::fs::write(&store, config.replace("trusted", "untrusted").as_bytes()).unwrap();
    assert_eq!(
        ADAPTER.workspace_trust(&workspace, &env, None).unwrap(),
        Trust::Declined
    );
    std::fs::write(&store, b"[bad").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &env, None).is_err());
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

fn codex_environment(
    home: Option<&str>,
    codex_home: Option<&str>,
) -> crate::native::consent::Environment {
    crate::native::consent::Environment {
        home: home.map(Into::into),
        codex_home: codex_home.map(Into::into),
        ..Default::default()
    }
}

#[test]
fn the_codex_home_is_config_toml_under_codex_home_or_the_user_home() {
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    // Without `CODEX_HOME`, `.codex` under the user home.
    assert_eq!(
        Home::resolve(&codex_environment(Some(home_str), None))
            .unwrap()
            .config,
        home.path().join(".codex").join("config.toml")
    );
    // `CODEX_HOME` replaces that directory and is kept as spelled: a relative value stays
    // relative, and an empty one names the caller's working directory.
    let custom = home.path().join("custom-codex");
    assert_eq!(
        Home::resolve(&codex_environment(Some(home_str), custom.to_str()))
            .unwrap()
            .config,
        custom.join("config.toml")
    );
    assert_eq!(
        Home::resolve(&codex_environment(Some(home_str), Some("relative")))
            .unwrap()
            .config,
        std::path::Path::new("relative").join("config.toml")
    );
    assert_eq!(
        Home::resolve(&codex_environment(Some(home_str), Some("")))
            .unwrap()
            .config,
        std::path::PathBuf::from("config.toml")
    );
    // The user home is required even when `CODEX_HOME` names the home.
    let error = Home::resolve(&codex_environment(None, custom.to_str())).unwrap_err();
    assert_eq!(error.to_string(), "no user home for workspace trust lookup");
    // An explicitly empty `HOME` still wins over `USERPROFILE`.
    let env = crate::native::consent::Environment {
        home: Some("".into()),
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&env).unwrap().config,
        std::path::PathBuf::from("")
            .join(".codex")
            .join("config.toml")
    );
    assert_eq!(
        Home::resolve(&codex_environment(None, None))
            .unwrap_err()
            .to_string(),
        "no user home for workspace trust lookup"
    );
}

#[test]
fn the_memory_workspace_is_beside_the_config_of_the_codex_home() {
    // Only the Codex home decides; the other providers' variables are never read.
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    assert_eq!(
        memory_workspace(&codex_environment(Some(home_str), None)).unwrap(),
        home.path().join(".codex").join("memories")
    );
    let custom = home.path().join("custom-codex");
    assert_eq!(
        memory_workspace(&codex_environment(Some(home_str), custom.to_str())).unwrap(),
        custom.join("memories")
    );
    // A relative config path stays relative: the caller's working directory resolves it.
    assert_eq!(
        memory_workspace(&codex_environment(Some(home_str), Some("relative"))).unwrap(),
        std::path::Path::new("relative").join("memories")
    );
    assert_eq!(
        memory_workspace(&codex_environment(Some(home_str), Some(""))).unwrap(),
        std::path::Path::new("memories")
    );
    let error = memory_workspace(&codex_environment(None, Some("custom"))).unwrap_err();
    assert_eq!(error.to_string(), "no user home for workspace trust lookup");
    assert_eq!(
        super::super::codex_memory_workspace(&codex_environment(Some(home_str), custom.to_str()))
            .unwrap(),
        memory_workspace(&codex_environment(Some(home_str), custom.to_str())).unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_trust_keys_are_lowercased_and_the_store_path_is_not() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let mixed = tmp.path().join("Codex-Home");
    std::fs::create_dir(&mixed).unwrap();
    let env = consent::Environment {
        home: Some(tmp.path().as_os_str().to_owned()),
        codex_home: Some(mixed.as_os_str().to_owned()),
        ..Default::default()
    };
    let store = mixed.join("config.toml");
    let native = consent::native_key(&workspace).unwrap();
    let lowercase = native.to_lowercase();
    assert_ne!(
        native, lowercase,
        "the fixture needs a mixed-case workspace path"
    );
    // An exact lowercase entry is found first; a mixed-case entry is found as a fallback;
    // the evidence key is lowercase either way and the store path keeps its case.
    for entry in [&lowercase, &native] {
        let config = format!(
            "[projects.{}]\ntrust_level = \"trusted\"\n",
            serde_json::to_string(entry).unwrap()
        );
        crate::native::session::primitives::write_private(&store, config.as_bytes()).unwrap();
        let Trust::Trusted(evidence) = ADAPTER.workspace_trust(&workspace, &env, None).unwrap()
        else {
            panic!("trusted through {entry}");
        };
        assert_eq!(evidence.key, lowercase);
        assert_eq!(evidence.store, store);
        std::fs::remove_file(&store).unwrap();
    }
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
                        let (chars, units) =
                            diagnose::framed_prompt_sizes(&correlated_prompt(&prompt, &pending));
                        assert_eq!(pending.framed_prompt_chars, Some(chars), "{label}");
                        assert_eq!(pending.framed_prompt_units, Some(units), "{label}");
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
                        assert_idle_watcher(plan.watcher);
                        assert!(plan.environment_removals.is_empty(), "{label}");
                        let observed = crate::native::consent::observe(&directory);
                        if consent {
                            assert_eq!(observed.applied_marker(), Some(APPLIED_MARKER), "{label}");
                        } else {
                            assert!(observed.value().is_null(), "{label}: {observed:?}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn pending_turn_record_round_trips_with_and_without_framed_prompt_sizes() {
    let directory = tempfile::tempdir().unwrap();
    // The shape 0.4.14 writes: no size fields. It decodes with both absent.
    let legacy = r#"{"schema":1,"claim_token":"1700000000000-1","marker":"<!-- agent-bridge-codex-turn:1700000000000-1 -->"}"#;
    std::fs::write(directory.path().join(PENDING_TURN_FILE), legacy).unwrap();
    let pending = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(pending.claim_token, "1700000000000-1");
    assert_eq!(pending.framed_prompt_chars, None);
    assert_eq!(pending.framed_prompt_units, None);
    let encoded = serde_json::to_value(&pending).unwrap();
    assert!(encoded.get("framed_prompt_chars").is_none());
    assert!(encoded.get("framed_prompt_units").is_none());

    // A record with the sizes keeps schema 1 and reads back exactly.
    let written =
        install_pending_turn_for_prompt(directory.path(), "1700000000000-2", "a\"b😀").unwrap();
    let framed = correlated_prompt("a\"b😀", &written);
    let (chars, units) = diagnose::framed_prompt_sizes(&framed);
    assert_eq!(written.framed_prompt_chars, Some(chars));
    assert_eq!(written.framed_prompt_units, Some(units));
    // The sizes are of the text before delivery: a non-BMP character counts one char and
    // two UTF-16 units (plus the two quotes of the JSON string), although the Windows
    // console paste can drop it (#147).
    assert_eq!(diagnose::framed_prompt_sizes("😀"), (1, 4));
    let text = std::fs::read_to_string(directory.path().join(PENDING_TURN_FILE)).unwrap();
    let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(raw["schema"], 1);
    assert_eq!(raw["framed_prompt_chars"], chars);
    assert_eq!(raw["framed_prompt_units"], units);
    let read = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(read.framed_prompt_chars, Some(chars));
    assert_eq!(read.framed_prompt_units, Some(units));
    // A binary that does not know the fields decodes the same text: the struct denies no
    // unknown field, so the 0.4.14 shape with an extra field still reads.
    #[derive(serde::Deserialize)]
    struct Legacy {
        schema: u32,
        claim_token: String,
        marker: String,
    }
    let legacy: Legacy = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (legacy.schema, legacy.claim_token.as_str()),
        (1, "1700000000000-2")
    );
    assert_eq!(legacy.marker, read.marker);
}

#[test]
fn lifecycle_binding_and_stop_keep_the_claim_pending() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path();
    crate::native::session::update_status(
        directory,
        crate::native::session::SessionState::Working,
        None,
        None,
    )
    .unwrap();
    std::fs::create_dir(directory.join("events")).unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory), &[]).unwrap();
    let pending = install_pending_turn(directory, claim.token()).unwrap();
    let prompt = correlated_prompt(
        &crate::native::native_delegation_prompt("external", "hello"),
        &pending,
    );
    let payload = serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":"01234567-89ab-cdef-0123-456789abcdef", "turn_id":"turn-1", "transcript_path":"rollout.jsonl", "prompt":prompt});
    ADAPTER.handle_hook(directory, &payload).unwrap();
    assert!(
        directory
            .join(format!("codex-turn-{}.json", claim.token()))
            .exists()
    );
    ADAPTER.handle_hook(directory, &serde_json::json!({"hook_event_name":"Stop", "session_id":payload["session_id"], "turn_id":"turn-1", "last_assistant_message":"candidate", "stop_hook_active":false})).unwrap();
    assert!(
        Reader::open_unchecked(directory)
            .events()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory))
            .unwrap()
            .as_deref(),
        Some(claim.token())
    );
}
