use super::*;
use crate::native::process_env::tests::{
    CODEX_CONTEXT_KEPT, NON_CLAUDE_CALLER_MARKERS, NON_CLAUDE_CONTEXT_KEPT,
};
use crate::native::session::turn;

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
    let prompt = crate::native::delegation_prompt("review this");
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
                            assert!(
                                framed.starts_with(&format!(
                                    "{}\n\n{prompt}\n\n",
                                    super::correlation::DELEGATION_HEADER
                                )),
                                "{label}"
                            );
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
                        assert_eq!(
                            plan.environment_removals,
                            &NON_CLAUDE_CALLER_MARKERS[..],
                            "{label}"
                        );
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
fn herdr_codex_launch_keeps_tools_in_the_terminal_process() {
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.169.0"));
    Store::open_unchecked(&directory)
        .private("terminal.json")
        .write_json(&serde_json::json!({
            "terminal": "herdr", "session_id": "t99",
            "managed_session_id": "session-hooks"
        }))
        .unwrap();
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/tabcli"),
            directory: &directory,
            workspace: root.path(),
            title: "Herdr Codex",
            prompt: "test task",
            yolo: false,
            model: None,
            effort: None,
        },
        false,
        |_, _| Ok(false),
    )
    .unwrap();
    assert_eq!(
        plan.arguments
            .iter()
            .filter(|arg| *arg == "--no-daemon")
            .count(),
        1,
        "Herdr requires Codex tool commands to run inside its terminal"
    );
    assert_eq!(
        plan.initial_prompt,
        InitialPromptDisposition::AcceptedAtSpawn
    );
}

#[test]
fn prepare_launch_refuses_windows_herdr_before_installing_pending_turn() {
    use crate::native::failure_kind::{FailureKind, classify};

    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.169.0"));
    Store::open_unchecked(&directory)
        .private("terminal.json")
        .write_json(&serde_json::json!({
            "terminal": "herdr", "session_id": "t99",
            "managed_session_id": "session-hooks"
        }))
        .unwrap();
    let error = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new(r"C:\tabcli.exe"),
            directory: &directory,
            workspace: root.path(),
            title: "Windows Herdr Codex",
            prompt: "test task",
            yolo: false,
            model: None,
            effort: None,
        },
        true,
        |_, _| Ok(false),
    )
    .err()
    .expect("Windows Herdr launch plan must refuse Codex");
    assert_eq!(classify(&error), Some(FailureKind::Unsupported));
    assert_eq!(format!("{error:#}"), CODEX_WINDOWS_HERDR_UNSUPPORTED);
    assert!(read_pending_turn(&directory).unwrap().is_none());
}

// The Herdr gate's version grammar (`herdr_codex_version`) as a table over its dimensions,
// each cell checked through pre-creation admission and the recorded-launch check, which
// always agree:
// - label present once: the verdict is the field's, whatever precedes the label (nothing, a
//   warning line, a timestamp on the label's line, a timestamped warning line, another
//   program's version or a prerelease on its own line) and whatever follows the field
//   (nothing, another version on its line, another version line);
// - label absent (`unlabelled_codex_version`): the text names a version only when it has
//   exactly one candidate (a word that, after its leading punctuation and one optional `v`,
//   begins with digits.digits.digits), whether or not the others would parse, and that
//   candidate has a reading (trailing characters that cannot end a version removed until it
//   is SemVer); otherwise it is refused (owner's decisions 4B and B2, 2026-11-26);
// - label layout: a second label, or a label without a field on its line, refuses.
#[test]
fn herdr_version_grammar_reads_only_the_labelled_field() {
    use crate::native::failure_kind::{FailureKind, classify};
    use crate::native::provider::SurfaceRefusal;
    use terminal::TerminalKind;

    const REFUSAL: &str = "Herdr requires stable Codex 0.169.0 or newer with --no-daemon;";
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.169.0"));
    let store = Store::open_unchecked(&directory);
    store
        .private("terminal.json")
        .write_json(&serde_json::json!({"terminal":"herdr","session_id":"t1"}))
        .unwrap();
    let admits = |text: &str| -> bool {
        let admission = surface_route(false, TerminalKind::Herdr, Some(text));
        // The fixture's manifest, with this cell's text as the recorded version.
        store
            .private("manifest.json")
            .write_json(&serde_json::json!({
                "schema": 1, "id": "session-hooks", "provider": "codex", "provider_path": "codex",
                "provider_version": text, "workspace": root.path(), "title": "hooks fixture",
                "yolo": false, "created_unix_ms": 1
            }))
            .unwrap();
        let recorded = recorded_surface_route(&directory, false);
        match (admission, recorded) {
            (Ok(admitted), Ok(launched)) => {
                assert_eq!(admitted, SurfaceRoute::LaunchArgument { no_daemon: true });
                assert_eq!(launched, admitted, "{text:?}");
                true
            }
            (Err(SurfaceRefusal::Version(detail)), Err(error)) => {
                assert!(
                    detail.starts_with(&format!("{REFUSAL} version: {text}")),
                    "{detail}"
                );
                assert!(
                    format!("{error:#}")
                        .starts_with(&format!("{REFUSAL} recorded version: {text}")),
                    "{error:#}"
                );
                assert_eq!(classify(&error), Some(FailureKind::Unsupported), "{text:?}");
                false
            }
            (admission, recorded) => panic!(
                "admission and the recorded launch disagree on {text:?}: {admission:?} / {:?}",
                recorded.map_err(|error| format!("{error:#}"))
            ),
        }
    };

    // The field; whether Herdr admits it read whole after the label; whether it is a
    // candidate; and the verdict on its reading without the label (`None`: no reading).
    let fields = [
        ("0.169.0", true, true, Some(true)),  // stable, at the floor
        ("0.170.0", true, true, Some(true)),  // stable, above the floor
        ("v0.170.0", true, true, Some(true)), // with the optional `v`
        ("0.170.0+build.1", true, true, Some(true)), // build metadata is not a prerelease
        ("0.168.0", false, true, Some(false)), // below the floor
        ("0.170.0-alpha.1", false, true, Some(false)), // prerelease
        ("0.170.0--", false, true, Some(false)), // prerelease `-`, kept as written
        ("0.170.0-", false, true, None),      // its `-` names no prerelease
        ("0.170.0-alpha.1.", false, true, Some(false)), // a period after a prerelease
        ("0.168.0-rc-.", false, true, Some(false)), // a period after the prerelease `rc-`
        ("0.170.0+", false, true, Some(true)), // an empty build marker
        ("0.170.0.", false, true, Some(true)), // a sentence's period
        ("0.170.0-01", false, true, None),    // a leading zero in a prerelease
        ("(0.170.0)", false, true, Some(true)), // brackets
        ("unknown", false, false, None),      // not a candidate
        ("0.170", false, false, None),        // two numbers are not a candidate
        ("0.170.0.1", false, false, None),    // four plain numbers are not a candidate
        ("127.0.0.1:8080", false, false, None), // nor are an IPv4 address and port
        ("v0.170.0.1", false, true, None),    // four numbers with a `v` are an unreadable one
        ("0.170.0.1-rc.1", false, true, None), // and with a prerelease
        ("codex/0.168.0", false, true, None), // a version inside a word is one too
    ];
    let before = [
        "",
        "warning: config.toml is deprecated\n",
        "2026-11-25T10:00:00Z ",
        "2026-11-25T10:00:00Z WARN slow start after 1.0.0\n",
        "node v22.0.0\n",
        "plugin 0.160.0-beta.1\n",
    ];
    let after = [
        "",
        " (runtime 22.0.0)",
        " (protocol 0.1.0)",
        "\nnode v22.0.0",
    ];
    for (field, admitted, candidate, reading) in fields {
        for prefix in before {
            for suffix in after {
                let text = format!("{prefix}codex-cli {field}{suffix}");
                assert_eq!(admits(&text), admitted, "{text:?}");
            }
        }
        // Without the label the field decides when it is the only candidate. Beside another
        // candidate (`22.0.0` here, or the field repeated) the text is ambiguous, whether or
        // not the field reads as a version; where the field is no candidate, that other
        // version is the only one.
        let alone = candidate && reading == Some(true);
        for (unlabelled, expected) in [
            (field.to_owned(), alone),
            (format!("\n{field}\n"), alone),
            (format!("codex {field}"), alone),
            (format!("OpenAI Codex ({field})"), alone),
            (
                format!("OpenAI Codex {field}."),
                alone && !field.ends_with('-'),
            ),
            (format!("{field} (runtime 22.0.0)"), !candidate),
            (format!("node v22.0.0\n{field}"), !candidate),
            (format!("codex {field} ({field})"), false),
        ] {
            assert_eq!(admits(&unlabelled), expected, "{unlabelled:?}");
        }
        // A second label, or a label whose field is on another line, refuses.
        for layout in [
            format!("codex-cli {field}\ncodex-cli {field}"),
            format!("warning: codex-cli is outdated\ncodex-cli {field}"),
            format!("codex-cli\n{field}"),
        ] {
            assert!(!admits(&layout), "{layout:?}");
        }
    }
    assert!(!admits("codex-cli"));
    assert!(!admits(""));
}

// The readers of the released wrappers, frozen to state the handoffs of a pending launch:
// what a wrapper of that release does with a Herdr record another release wrote. Each answers
// whether the release launches Codex on Herdr with the recorded `--version` text.
mod released {
    use semver::Version;

    const FLOOR: Version = Version::new(0, 169, 0);

    fn admitted(version: Option<Version>) -> bool {
        version.is_some_and(|version| version >= FLOOR && version.pre.is_empty())
    }

    // 0.4.48: the first token that parses once its surrounding punctuation and one leading
    // `v` are removed.
    pub(super) fn v0_4_48(text: &str) -> bool {
        admitted(text.split_whitespace().find_map(|token| {
            let trimmed = token.trim_matches(|character: char| !character.is_ascii_alphanumeric());
            Version::parse(trimmed.strip_prefix('v').unwrap_or(trimmed)).ok()
        }))
    }

    // 0.4.49: the word after a single `codex-cli` label, or a text of one word, read whole.
    pub(super) fn v0_4_49(text: &str) -> bool {
        let lines = text
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let labels = lines
            .iter()
            .flat_map(|words| {
                words
                    .iter()
                    .enumerate()
                    .filter(|(_, word)| **word == "codex-cli")
                    .map(move |(index, _)| words.get(index + 1).copied())
            })
            .collect::<Vec<_>>();
        let field = match labels.as_slice() {
            [Some(field)] => *field,
            [] => match lines.concat().as_slice() {
                [word] => *word,
                _ => return false,
            },
            _ => return false,
        };
        admitted(Version::parse(field.strip_prefix('v').unwrap_or(field)).ok())
    }
}

// The handoff table of docs/agents/codex.md, one row per `--version` text: whether 0.4.48,
// 0.4.49 and this build launch Codex on Herdr with it. The document holds the table, and this
// test reads it, so that the document cannot disagree with the readers.
fn handoff_table() -> Vec<(String, [bool; 3])> {
    handoff_rows(include_str!("../../../../docs/agents/codex.md")).unwrap()
}

// The rows between the table's markers: its header and separator, then rows of a code-spanned
// text and three verdicts. Any other nonempty line is an error, so that no row escapes the
// checks by its form.
fn handoff_rows(document: &str) -> std::result::Result<Vec<(String, [bool; 3])>, String> {
    const BEGIN: &str = "<!-- herdr-version-handoff -->";
    const END: &str = "<!-- /herdr-version-handoff -->";
    const HEADER: &str = "| `--version` text | 0.4.48 | 0.4.49 | this build |";
    const SEPARATOR: &str = "| --- | --- | --- | --- |";
    let start = document.find(BEGIN).ok_or("no table start")? + BEGIN.len();
    let end = document[start..].find(END).ok_or("no table end")? + start;
    let mut lines = document[start..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if lines.next() != Some(HEADER) || lines.next() != Some(SEPARATOR) {
        return Err("the table does not start with its header and separator".into());
    }
    lines
        .map(|line| {
            let cells = line
                .strip_prefix('|')
                .and_then(|line| line.strip_suffix('|'))
                .map(|line| line.split('|').map(str::trim).collect::<Vec<_>>())
                .filter(|cells| cells.len() == 4)
                .ok_or_else(|| format!("not a table row: {line}"))?;
            let text = cells[0]
                .strip_prefix('`')
                .and_then(|text| text.strip_suffix('`'))
                .filter(|text| !text.is_empty() && !text.contains('`'))
                .ok_or_else(|| format!("the text is not one code span: {line}"))?;
            let verdict = |cell: &str| match cell {
                "launches" => Ok(true),
                "refuses" => Ok(false),
                other => Err(format!("unknown verdict {other:?} in {line}")),
            };
            Ok((
                text.replace("\\n", "\n"),
                [verdict(cells[1])?, verdict(cells[2])?, verdict(cells[3])?],
            ))
        })
        .collect()
}

// The table reader refuses a row it does not recognise instead of skipping it.
#[test]
fn the_handoff_table_reader_refuses_an_unrecognised_row() {
    let table = |rows: &str| {
        format!(
            "<!-- herdr-version-handoff -->\n| `--version` text | 0.4.48 | 0.4.49 | this build |\n| --- | --- | --- | --- |\n{rows}\n<!-- /herdr-version-handoff -->"
        )
    };
    assert_eq!(
        handoff_rows(&table("| `0.170.0` | launches | launches | launches |")).unwrap(),
        vec![("0.170.0".to_owned(), [true, true, true])]
    );
    for row in [
        "|`0.170.0`|launches|launches|launches|",
        "| 0.170.0 | launches | launches | launches |",
        "| `0.170.0` | launches | launches |",
        "| `0.170.0` | launches | maybe | launches |",
        "`0.170.0` launches launches launches",
    ] {
        let rows = handoff_rows(&table(row));
        if row.starts_with("|`") {
            // Spacing is not form: the row is read.
            assert_eq!(rows.unwrap().len(), 1, "{row}");
        } else {
            assert!(rows.is_err(), "{row}");
        }
    }
    assert!(handoff_rows("| `0.170.0` | launches | launches | launches |").is_err());
}

// A sole candidate is read in time linear in its length, whatever trailing punctuation it has.
#[test]
fn a_long_candidate_is_read_in_linear_time() {
    let word = format!("0.170.0+{}{}", "a".repeat(100_000), ".".repeat(100_000));
    let started = std::time::Instant::now();
    let version = herdr_codex_version(&word).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(version.build.as_str().len(), 100_000);
}

// A pending Herdr launch can be started by a wrapper of another release than the one that
// recorded it (the binary at the launch's executable path was replaced in between). Every row
// of the document's handoff table holds for the frozen readers and this build's admission and
// recorded-launch check, and the table holds every text the reviews of #413 named.
#[test]
fn pending_herdr_launches_across_the_wrapper_handoff() {
    use terminal::TerminalKind;
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.169.0"));
    let store = Store::open_unchecked(&directory);
    store
        .private("terminal.json")
        .write_json(&serde_json::json!({"terminal":"herdr","session_id":"t1"}))
        .unwrap();
    let launches = |text: &str| {
        store
            .private("manifest.json")
            .write_json(&serde_json::json!({
                "schema": 1, "id": "session-hooks", "provider": "codex", "provider_path": "codex",
                "provider_version": text, "workspace": root.path(), "title": "hooks fixture",
                "yolo": false, "created_unix_ms": 1
            }))
            .unwrap();
        let launched = recorded_surface_route(&directory, false)
            .is_ok_and(|route| route == SurfaceRoute::LaunchArgument { no_daemon: true });
        assert_eq!(
            launched,
            surface_route(false, TerminalKind::Herdr, Some(text)).is_ok(),
            "{text:?}"
        );
        launched
    };
    let table = handoff_table();
    for (text, [by_0_4_48, by_0_4_49, by_this_build]) in &table {
        assert_eq!(released::v0_4_48(text), *by_0_4_48, "0.4.48 on {text:?}");
        assert_eq!(released::v0_4_49(text), *by_0_4_49, "0.4.49 on {text:?}");
        assert_eq!(launches(text), *by_this_build, "this build on {text:?}");
    }
    for named in [
        "OpenAI Codex (v0.170.0)",
        "(0.170.0)",
        "0.170.0+",
        "0.170.0--",
        "(0.170.0--)",
        "0.170.0-",
        "codex-cli 0.170.0+",
        "node v22.0.0\ncodex-cli 0.168.0",
        "plugin 0.160.0\ncodex-cli 0.170.0",
        "node v22.0.0\nOpenAI Codex (v0.170.0)",
        "codex 0.170.0 (v0.170.0)",
        "unknown (runtime 22.0.0)",
        "0.168.0- runtime 22.0.0",
        "OpenAI Codex 0.168.0-rc-. (runtime 22.0.0)",
        "0.170.0-01 (runtime 22.0.0)",
        "OpenAI Codex 0.170.0 (proxy 127.0.0.1:8080)",
        "codex 0.170.0 build 1.2.3.4",
        "Codex <0.169.0",
        "OpenAI Codex (v0.168.0.1-rc.1) (runtime 22.0.0)",
        "codex/0.168.0 (node 22.0.0)",
    ] {
        assert!(
            table.iter().any(|(text, _)| text == named),
            "the handoff table lacks {named:?}"
        );
    }
}

// Each refusal of the grammar names what refused: the field's own parse error, a missing
// label, a label without a field, a second label, no candidate, a candidate without a
// reading, or several candidates.
#[test]
fn herdr_version_refusals_name_the_rule_that_refused() {
    let refusal = |text: &str| format!("{:#}", herdr_codex_version(text).unwrap_err());
    for (text, field) in [
        ("codex-cli 0.170.0+", "0.170.0+"),
        ("codex-cli unknown (runtime 22.0.0)", "unknown"),
        (
            "2026-11-25T10:00:00Z codex-cli 0.170.0. (runtime 22.0.0)",
            "0.170.0.",
        ),
    ] {
        let error = refusal(text);
        assert!(
            error.starts_with(&format!(
                "Codex version {field:?} is not a semantic version: "
            )),
            "{text:?}: {error}"
        );
    }
    // A sole candidate is read with its prerelease or build, less trailing characters that
    // cannot end a version.
    for text in [
        "OpenAI Codex (v0.170.0)",
        "(0.170.0)",
        "0.170.0+",
        "Codex 0.170.0.",
    ] {
        assert_eq!(
            herdr_codex_version(text).unwrap(),
            Version::new(0, 170, 0),
            "{text}"
        );
    }
    for (text, prerelease) in [
        ("0.170.0--", "-"),
        ("(0.170.0--)", "-"),
        ("OpenAI Codex (v0.170.0--)", "-"),
        ("OpenAI Codex 0.168.0-rc-.", "rc-"),
    ] {
        assert_eq!(
            herdr_codex_version(text).unwrap().pre.as_str(),
            prerelease,
            "{text}"
        );
    }
    for text in ["OpenAI Codex (unknown)", "unknown", ""] {
        assert_eq!(
            refusal(text),
            format!("found no `codex-cli` label and no version in {text:?}")
        );
    }
    assert_eq!(
        refusal("0.170.0-"),
        "found no `codex-cli` label, and its one version \"0.170.0-\" is not a semantic version"
    );
    for (text, candidates) in [
        (
            "node v22.0.0\nOpenAI Codex (v0.170.0)",
            "\"v22.0.0\", \"(v0.170.0)\"",
        ),
        ("codex 0.170.0 (v0.170.0)", "\"0.170.0\", \"(v0.170.0)\""),
        ("0.168.0- runtime 22.0.0", "\"0.168.0-\", \"22.0.0\""),
        (
            "OpenAI Codex 0.168.0-rc-. (runtime 22.0.0)",
            "\"0.168.0-rc-.\", \"22.0.0)\"",
        ),
    ] {
        assert_eq!(
            refusal(text),
            format!(
                "found no `codex-cli` label and 2 versions ({candidates}) in {text:?}; TAB does not guess which one is Codex's"
            )
        );
    }
    assert_eq!(
        refusal("codex-cli\n0.170.0"),
        "the `codex-cli` label has no version after it on its line in \"codex-cli\\n0.170.0\""
    );
    assert_eq!(
        refusal("codex-cli 0.170.0\ncodex-cli 0.170.0"),
        "found the `codex-cli` label more than once in \"codex-cli 0.170.0\\ncodex-cli 0.170.0\""
    );
    let prerelease = herdr_codex_version("codex-cli 0.170.0--").unwrap();
    assert_eq!(prerelease.pre.as_str(), "-");
    let build = herdr_codex_version("codex-cli 0.170.0+build.1").unwrap();
    assert!(build.pre.is_empty());
    assert_eq!(build.build.as_str(), "build.1");
}

// The queue floor, hook eligibility and the diagnostics read a recorded or probed version as
// the general probe and every earlier release did, so a launch record v0.4.48 admitted keeps
// its meaning. Only the Herdr gate reads more strictly.
#[test]
fn recorded_versions_keep_the_reading_of_earlier_releases() {
    for (text, expected) in [
        ("codex-cli 0.170.0", Version::new(0, 170, 0)),
        ("Codex version is 0.170.0.", Version::new(0, 170, 0)),
        ("codex-cli 0.170.0+", Version::new(0, 170, 0)),
        ("codex-cli 0.170.0-", Version::new(0, 170, 0)),
        ("OpenAI Codex (v0.170.0).", Version::new(0, 170, 0)),
        (
            "codex-cli 0.168.0+ (runtime 22.0.0)",
            Version::new(0, 168, 0),
        ),
        (
            "codex-cli 0.168.0- (protocol 1.0.0)",
            Version::new(0, 168, 0),
        ),
    ] {
        assert_eq!(codex_version(text).unwrap(), expected, "{text}");
        assert!(codex_version_supports_native_queue(text).unwrap(), "{text}");
    }
    assert!(!codex_version_supports_native_queue("Codex version is 0.148.9.").unwrap());
}

#[test]
fn surface_route_unifies_admission_version_floor_and_initial_transport_across_platforms() {
    use crate::native::provider::SurfaceRefusal;
    use terminal::TerminalKind;

    for windows in [false, true] {
        let default_transport = SurfaceRoute::platform_default(windows)
            .initial_transport()
            .facts();
        for surface in TerminalKind::ALL {
            for (version, herdr_version_ok) in [
                (None, true),
                (Some("codex-cli 0.147.0"), false),
                (Some("codex-cli 0.168.0"), false),
                (Some("codex-cli 0.169.0-alpha.1"), false),
                (Some("codex-cli 0.170.0-alpha.1"), false),
                (Some("codex-cli 0.170.0--"), false),
                (Some("codex-cli 0.170.0+"), false),
                (Some("codex-cli 0.170.0-"), false),
                (Some("codex-cli 0.168.0+ (runtime 22.0.0)"), false),
                (Some("codex-cli 0.168.0- (protocol 1.0.0)"), false),
                (Some("codex-cli 0.170.0-01 (runtime 22.0.0)"), false),
                (Some("codex-cli 0.169.0"), true),
                (Some("codex-cli 0.170.0"), true),
                (Some("codex-cli 0.170.0+build.1"), true),
                (Some("codex-cli 0.170.0 (runtime 22.0.0)"), true),
                (Some("unknown"), false),
            ] {
                let result = surface_route(windows, surface, version);
                match (windows, surface) {
                    (true, TerminalKind::Herdr) => {
                        assert_eq!(
                            result,
                            Err(SurfaceRefusal::Platform(CODEX_WINDOWS_HERDR_UNSUPPORTED)),
                            "windows={windows} surface={surface:?} version={version:?}"
                        );
                    }
                    (true, _) => {
                        let route = result.unwrap();
                        assert_eq!(route, SurfaceRoute::ConsolePaste);
                        assert!(!route.no_daemon());
                        assert!(!route.passes_prompt_argument());
                        let facts = route.initial_transport().facts();
                        assert_eq!(facts.name, default_transport.name);
                        assert_eq!(facts.readiness, default_transport.readiness);
                        assert!(facts.surface.is_some());
                    }
                    (false, TerminalKind::Herdr) if !herdr_version_ok => {
                        let Err(SurfaceRefusal::Version(detail)) = result else {
                            panic!(
                                "expected version refusal for non-Windows Herdr {version:?}: {result:?}"
                            );
                        };
                        let version_text = version.unwrap();
                        assert!(
                            detail.starts_with(&format!(
                                "Herdr requires stable Codex 0.169.0 or newer with --no-daemon; version: {version_text}"
                            )),
                            "{detail}"
                        );
                    }
                    (false, TerminalKind::Herdr) => {
                        let route = result.unwrap();
                        assert_eq!(route, SurfaceRoute::LaunchArgument { no_daemon: true });
                        assert!(route.no_daemon());
                        assert!(route.passes_prompt_argument());
                        let facts = route.initial_transport().facts();
                        assert_eq!(facts.name, default_transport.name);
                        assert_eq!(facts.readiness, default_transport.readiness);
                        assert!(facts.surface.is_none());
                    }
                    (false, _) => {
                        let route = result.unwrap();
                        assert_eq!(route, SurfaceRoute::LaunchArgument { no_daemon: false });
                        assert!(!route.no_daemon());
                        assert!(route.passes_prompt_argument());
                        let facts = route.initial_transport().facts();
                        assert_eq!(facts.name, default_transport.name);
                        assert_eq!(facts.readiness, default_transport.readiness);
                        assert!(facts.surface.is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn herdr_runtime_selection_uses_the_surface_and_requires_a_supported_codex() {
    use crate::native::failure_kind::{FailureKind, classify};

    let prepare = |directory: &Path, workspace: &Path, windows: bool| {
        prepare_launch_for_platform(
            LaunchContext {
                bridge_executable: Path::new("/opt/tabcli"),
                directory,
                workspace,
                title: "Codex surface test",
                prompt: "test task",
                yolo: false,
                model: None,
                effort: None,
            },
            windows,
            |_, _| Ok(false),
        )
    };

    for (version, accepted) in [
        ("codex-cli 0.168.0", false),
        ("codex-cli 0.169.0-alpha.1", false),
        ("codex-cli 0.170.0-alpha.1", false),
        ("codex-cli 0.170.0--", false),
        ("codex-cli 0.170.0+", false),
        ("codex-cli 0.168.0+ (runtime 22.0.0)", false),
        ("codex-cli 0.168.0- (protocol 1.0.0)", false),
        ("codex-cli 0.170.0-01 (runtime 22.0.0)", false),
        ("codex-cli 0.169.0", true),
        ("codex-cli 0.170.0", true),
        ("unknown", false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), Some(version));
        let store = Store::open_unchecked(&directory);
        store
            .private("terminal.json")
            .write_json(&serde_json::json!({"terminal":"herdr","session_id":"t1"}))
            .unwrap();
        let herdr_plan = prepare(&directory, root.path(), false);
        assert_eq!(herdr_plan.is_ok(), accepted, "{version}");
        if let Ok(plan) = herdr_plan {
            assert_eq!(
                plan.arguments
                    .iter()
                    .filter(|arg| *arg == "--no-daemon")
                    .count(),
                1,
                "{version}"
            );
            assert_eq!(
                plan.initial_prompt,
                InitialPromptDisposition::AcceptedAtSpawn,
                "{version}"
            );
        } else {
            let error = herdr_plan.err().unwrap();
            let detail = format!("{error:#}");
            assert!(
                detail.starts_with(&format!(
                    "Herdr requires stable Codex 0.169.0 or newer with --no-daemon; recorded version: {version}"
                )),
                "{detail}"
            );
            assert_eq!(
                classify(&error),
                Some(FailureKind::Unsupported),
                "{version}"
            );
            assert!(
                read_pending_turn(&directory).unwrap().is_none(),
                "{version}"
            );
        }

        // The same provider version on a non-Herdr surface has no 0.169.0 floor or --no-daemon.
        store
            .private("terminal.json")
            .write_json(&serde_json::json!({"terminal":"iterm2","session_id":"pane"}))
            .unwrap();
        let non_herdr_plan = prepare(&directory, root.path(), false).unwrap();
        assert!(
            !non_herdr_plan
                .arguments
                .iter()
                .any(|arg| arg == "--no-daemon"),
            "{version}"
        );
        assert_eq!(
            non_herdr_plan.initial_prompt,
            InitialPromptDisposition::AcceptedAtSpawn,
            "{version}"
        );

        // Native Windows managed console uses ConsolePaste for every supported version.
        store
            .private("terminal.json")
            .write_json(&serde_json::json!({"terminal":"windows-console","session_id":"console"}))
            .unwrap();
        let console_plan = prepare(&directory, root.path(), true).unwrap();
        assert!(
            !console_plan
                .arguments
                .iter()
                .any(|arg| arg == "--no-daemon" || arg == "-C"),
            "{version}"
        );
        assert_eq!(
            console_plan.initial_prompt,
            InitialPromptDisposition::AwaitingDelivery,
            "{version}"
        );
    }

    // Without a bound surface record, launch planning uses the platform's default route;
    // a malformed terminal record fails closed without installing a pending turn.
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        recorded_surface_route(root.path(), false).unwrap(),
        SurfaceRoute::LaunchArgument { no_daemon: false }
    );
    assert_eq!(
        recorded_surface_route(root.path(), true).unwrap(),
        SurfaceRoute::ConsolePaste
    );
    let broken_dir = test_support::launch_session(root.path(), Some("codex-cli 0.169.0"));
    Store::open_unchecked(&broken_dir)
        .private("terminal.json")
        .write_private(b"{broken")
        .unwrap();
    let error = prepare(&broken_dir, root.path(), false)
        .err()
        .expect("malformed terminal.json must fail closed");
    assert_eq!(classify(&error), None);
    assert!(read_pending_turn(&broken_dir).unwrap().is_none());
}

// Launches and probes drop both providers' caller identities while preserving configuration.
#[test]
fn launch_and_probe_drop_caller_identities_and_keep_configuration() {
    assert_eq!(
        ADAPTER.probe_environment_removals(),
        &NON_CLAUDE_CALLER_MARKERS[..]
    );
    for kept in CODEX_CONTEXT_KEPT
        .into_iter()
        .chain(NON_CLAUDE_CONTEXT_KEPT)
    {
        assert!(
            !ADAPTER.probe_environment_removals().contains(&kept),
            "{kept}"
        );
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
    let prompt = correlated_prompt(&crate::native::delegation_prompt("hello"), &pending);
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
