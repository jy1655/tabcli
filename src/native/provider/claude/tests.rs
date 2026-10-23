use super::turn::ClaudeTurn;
const PENDING_TURN_FILE: &str = "claude-pending-turn.json";
use crate::native::session::{Store, turn};
use std::{path::PathBuf, time::Duration};

#[test]
fn git_probe_drops_caller_session_environment() {
    crate::native::process_env::tests::assert_helper_environment(
        &super::trust::git_workspace_command(std::path::Path::new("/workspace")).unwrap(),
        &[],
    );
}

#[cfg(windows)]
fn windows_workspace_trust_screen(displayed_workspace: &str, yes_selected: bool) -> String {
    // Dialog text and wrapping from the issue #28 native Windows capture, Claude Code 2.1.302.
    let (no, yes) = if yes_selected {
        ("No, exit", "❯ Yes, I trust this folder")
    } else {
        ("❯ No, exit", "Yes, I trust this folder")
    };
    format!(
        "\n────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────\n Accessing workspace:\n\n {displayed_workspace}\n\n Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source\n project, or work from your team). If not, take a moment to review what's in this folder first.\n\n Claude Code'll be able to read, edit, and execute files here.\n\n Security guide\n\n {no}\n   {yes}\n\n Enter to confirm · Esc to cancel\n\n"
    )
}

#[cfg(windows)]
#[test]
fn windows_workspace_trust_dialog_accepts_the_observed_native_path() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let native = crate::native::consent::native_key(&workspace).unwrap();
    assert!(native.contains('\\'));
    for (yes_selected, key) in [
        (false, terminal::DialogKey::DownEnter),
        (true, terminal::DialogKey::Enter),
    ] {
        assert_eq!(
            ADAPTER.workspace_trust_key(
                &windows_workspace_trust_screen(&native, yes_selected),
                &workspace,
            ),
            Some(key),
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_workspace_trust_dialog_keeps_forward_slash_compatibility() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let forward = crate::native::consent::native_key(&workspace)
        .unwrap()
        .replace('\\', "/");
    for (yes_selected, key) in [
        (false, terminal::DialogKey::DownEnter),
        (true, terminal::DialogKey::Enter),
    ] {
        assert_eq!(
            ADAPTER.workspace_trust_key(
                &windows_workspace_trust_screen(&forward, yes_selected),
                &workspace,
            ),
            Some(key),
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_workspace_trust_dialog_refuses_other_paths_and_extra_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let native = crate::native::consent::native_key(&workspace).unwrap();
    for displayed in [native.clone(), native.replace('\\', "/")] {
        for yes_selected in [false, true] {
            let screen = windows_workspace_trust_screen(&displayed, yes_selected);
            for other in [
                temp.path().join("unrelated"),
                temp.path().join("workspace-sibling"),
                workspace.join("child"),
            ] {
                assert_eq!(ADAPTER.workspace_trust_key(&screen, &other), None);
                let other_key = crate::native::consent::native_key(&other).unwrap();
                let other_key = if displayed.contains('/') {
                    other_key.replace('\\', "/")
                } else {
                    other_key
                };
                assert_eq!(
                    ADAPTER
                        .workspace_trust_key(&screen.replace(&displayed, &other_key), &workspace),
                    None,
                );
            }
            for alias in [
                format!("{displayed}/"),
                format!("{displayed}/../workspace"),
                displayed.to_uppercase(),
            ] {
                assert_ne!(alias, displayed);
                assert_eq!(
                    ADAPTER.workspace_trust_key(&screen.replace(&displayed, &alias), &workspace),
                    None,
                );
            }
            for extra_prompt in [
                format!("{screen}Allow Bash?\n"),
                screen.replace("Enter to confirm", "Allow Bash?\nEnter to confirm"),
            ] {
                assert_eq!(ADAPTER.workspace_trust_key(&extra_prompt, &workspace), None);
            }
        }
    }
}

#[test]
fn workspace_trust_and_dialog_require_the_exact_workspace() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let env = consent::Environment::at_home(tmp.path());
    let store = tmp.path().join(".claude.json");
    let native = consent::native_key(&workspace).unwrap();
    let key = if cfg!(windows) {
        native.replace('\\', "/")
    } else {
        native
    };
    crate::native::session::primitives::write_json_atomic(
        &store,
        &serde_json::json!({"projects":{key.clone():{"hasTrustDialogAccepted":true}}}),
    )
    .unwrap();
    // The evidence names the store the adapter read, spelled as its home resolves it.
    let Trust::Trusted(evidence) = ADAPTER.workspace_trust(&workspace, &env, None).unwrap() else {
        panic!("trusted");
    };
    assert_eq!(evidence.provider, "claude");
    assert_eq!(evidence.key, key);
    assert_eq!(evidence.store, store);
    let serialized = serde_json::to_value(&evidence).unwrap()["store"]
        .as_str()
        .unwrap()
        .to_owned();
    if cfg!(windows) {
        assert!(serialized.ends_with(r"\.claude.json"), "{serialized}");
    } else {
        assert!(serialized.ends_with("/.claude.json"), "{serialized}");
    }
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &env, None)
            .unwrap(),
        Trust::Absent
    );
    let screen = format!(
        "Accessing workspace:\n{key}\nQuick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to review what's in this folder first.\nClaude Code'll be able to read, edit, and execute files here.\nSecurity guide\n❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel"
    );
    assert_eq!(
        trust::dialog_key(&screen, &workspace),
        Some(terminal::DialogKey::DownEnter)
    );
    assert_eq!(trust::dialog_key(&screen, &workspace.join("child")), None);
    assert_eq!(
        trust::dialog_key(&(screen.clone() + "\nAllow Bash?"), &workspace),
        None
    );
    // Claude 2.1.284 shows cwd but persists this dialog's decision at the
    // Git root. An exact child consent must never approve the parent repo.
    let child = workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    let child_key = consent::native_key(&child).unwrap();
    let child_key = if cfg!(windows) {
        child_key.replace('\\', "/")
    } else {
        child_key
    };
    let child_screen = screen.replace(&key, &child_key);
    assert_eq!(trust::dialog_key(&child_screen, &child), None);
    assert!(ADAPTER.workspace_trust(&child, &env, None).is_err());
    assert!(matches!(
        ADAPTER.workspace_trust(&workspace, &env, None).unwrap(),
        Trust::Trusted(_)
    ));
    // A nested repository owns a distinct, exact approval scope.
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&child)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        trust::dialog_key(&child_screen, &child),
        Some(terminal::DialogKey::DownEnter)
    );
    std::fs::write(&store, b"{\"projects\":[]}").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &env, None).is_err());
}

// B1: an empty `CLAUDE_CONFIG_DIR` is unset for the trust store as it always was
// for the session registry; before, the trust store read it as the relative `.claude.json`.
#[test]
fn the_claude_home_has_two_defaults_and_one_override_for_both_paths() {
    use crate::native::consent::Environment;
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    let environment = |config_dir: Option<&str>| Environment {
        home: Some(home_str.into()),
        claude_config_dir: config_dir.map(Into::into),
        ..Default::default()
    };
    // Unset: two different parents.
    assert_eq!(
        trust::trust_store(&environment(None)).unwrap(),
        home.path().join(".claude.json")
    );
    assert_eq!(
        claude_sessions_registry_dir(&environment(None)).unwrap(),
        home.path().join(".claude").join("sessions")
    );
    // A non-empty override holds both and is kept as spelled, relative or not.
    let custom = home.path().join("custom-claude");
    assert_eq!(
        trust::trust_store(&environment(custom.to_str())).unwrap(),
        custom.join(".claude.json")
    );
    assert_eq!(
        claude_sessions_registry_dir(&environment(custom.to_str())).unwrap(),
        custom.join("sessions")
    );
    assert_eq!(
        trust::trust_store(&environment(Some("relative"))).unwrap(),
        Path::new("relative").join(".claude.json")
    );
    assert_eq!(
        claude_sessions_registry_dir(&environment(Some("relative"))).unwrap(),
        Path::new("relative").join("sessions")
    );
    // An empty override is unset for both.
    assert_eq!(
        trust::trust_store(&environment(Some(""))).unwrap(),
        home.path().join(".claude.json")
    );
    assert_eq!(
        claude_sessions_registry_dir(&environment(Some(""))).unwrap(),
        home.path().join(".claude").join("sessions")
    );
    // The two requirements differ: the registry succeeds on the override alone, the
    // trust store needs the user home even then.
    let override_only = Environment {
        claude_config_dir: Some(custom.as_os_str().to_owned()),
        ..Default::default()
    };
    assert_eq!(
        claude_sessions_registry_dir(&override_only).unwrap(),
        custom.join("sessions")
    );
    assert_eq!(
        trust::trust_store(&override_only).unwrap_err().to_string(),
        "no user home for workspace trust lookup"
    );
    assert_eq!(
        claude_sessions_registry_dir(&Environment::default())
            .unwrap_err()
            .to_string(),
        "neither HOME nor USERPROFILE is set"
    );
    // `HOME`, even empty, before `USERPROFILE`.
    let empty_home = Environment {
        home: Some("".into()),
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        trust::trust_store(&empty_home).unwrap(),
        PathBuf::from("").join(".claude.json")
    );
    assert_eq!(
        claude_sessions_registry_dir(&empty_home).unwrap(),
        PathBuf::from("").join(".claude").join("sessions")
    );
    let profile_only = Environment {
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        trust::trust_store(&profile_only).unwrap(),
        home.path().join(".claude.json")
    );
}

use super::*;

fn launch_preflight(directory: &Path) {
    Store::open_unchecked(directory)
        .write_manifest(&crate::native::session::schema::SessionManifest {
            schema: 1,
            id: directory.file_name().unwrap().to_str().unwrap().into(),
            provider: "claude".into(),
            provider_path: "claude".into(),
            provider_version: "2.1.302 (Claude Code)".into(),
            workspace: directory.into(),
            title: "Claude launch fixture".into(),
            model: None,
            effort: None,
            yolo: true,
            created_unix_ms: 1,
        })
        .unwrap();
}

const MOD_CONVERSATION: &str = "00000000-0000-4000-8000-000000000249";
const MOD_REQUEST: &str = "claude-turn-1000000000000000000-10000-1";

fn mod_regression_fixture(native: bool) -> (tempfile::TempDir, String, serde_json::Value) {
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir()
        .unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    let token = claim.token().to_owned();
    claim.retain();
    let start: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/249-start.json")).unwrap();
    let text = start["event"]["text"].as_str().unwrap();
    let expected = text
        .split_once(">\n")
        .unwrap()
        .1
        .strip_suffix("\n</cross-session-message>")
        .unwrap();
    let pending = ClaudeTurn::new(MOD_REQUEST).unwrap();
    Store::open_unchecked(directory.path())
        .private(PENDING_TURN_FILE)
        .write_json(&serde_json::json!({
            "schema": 1, "request_id": pending.id(), "marker": pending.marker(),
            "claim_token": token, "expected_message": expected,
        }))
        .unwrap();
    if native {
        Store::open_unchecked(directory.path()).private("claude-completion-mode.json").write_json(
            &serde_json::json!({"schema":1,"mode":"native_v1","reason":"windows_stable_2_1_302_or_newer","claude_version":"2.1.302"})
        ).unwrap();
    }
    (directory, token, start["event"].clone())
}

#[test]
fn fresh_macos_launch_ignores_draft_stop_until_native_completion() {
    let (directory, token, start) = mod_regression_fixture(false);
    launch_preflight(directory.path());
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: directory.path(),
            workspace: directory.path(),
            title: "macOS completion regression",
            prompt: "initial request",
            yolo: false,
            model: None,
            effort: None,
        },
        false,
    )
    .unwrap();
    let expected = start["text"]
        .as_str()
        .unwrap()
        .split_once(">\n")
        .unwrap()
        .1
        .strip_suffix("\n</cross-session-message>")
        .unwrap();
    completion::install_pending(directory.path(), MOD_REQUEST, Some(&token), expected)
        .unwrap()
        .retain();
    handle_hook(directory.path(), &serde_json::json!({"agent_bridge_claude_mod":1,"event":"turn.start","conversation":MOD_CONVERSATION,"data":start})).unwrap();
    let stop: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/249-stop-draft.json")).unwrap();
    handle_hook(directory.path(), &stop["event"]).unwrap();
    let reader = Reader::open_unchecked(directory.path());
    assert_eq!(
        reader.events().unwrap().len(),
        0,
        "fresh macOS launch published a draft Stop before native completion"
    );
    assert_eq!(
        turn::current_claim_token(&reader).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert!(completion::native_mode(directory.path()).unwrap());
    assert_eq!(
        plan.initial_prompt,
        InitialPromptDisposition::AwaitingDelivery
    );
    assert!(
        !plan
            .arguments
            .iter()
            .any(|argument| argument == "initial request")
    );
    let complete: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/249-complete.json")).unwrap();
    handle_hook(directory.path(), &serde_json::json!({"agent_bridge_claude_mod":1,"event":"turn.complete","conversation":MOD_CONVERSATION,"data":complete["event"]})).unwrap();
    let event = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        Some(MOD_REQUEST),
        Some(&token),
        Instant::now() + Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .unwrap();
    assert_eq!(event.message, "PROBE-FINAL");
    assert_eq!(event.turn_id.as_deref(), Some(MOD_REQUEST));
    assert_eq!(
        event.provenance.as_deref(),
        Some("claude_mod_turn_complete")
    );
    assert_eq!(reader.events().unwrap().len(), 1);
}

#[test]
fn native_mod_stop_draft_does_not_settle_the_claim() {
    let (directory, token, _) = mod_regression_fixture(true);
    let stop: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/249-stop-draft.json")).unwrap();
    handle_hook(directory.path(), &stop["event"]).unwrap();
    assert!(
        Reader::open_unchecked(directory.path())
            .events()
            .unwrap()
            .is_empty(),
        "a Stop draft is not a terminal turn"
    );
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path()))
            .unwrap()
            .as_deref(),
        Some(token.as_str())
    );
}

#[test]
fn native_mod_final_settles_with_the_bridge_wait_identity() {
    let (directory, token, start) = mod_regression_fixture(true);
    let mut pending: serde_json::Value = Reader::open_unchecked(directory.path())
        .private(PENDING_TURN_FILE)
        .json()
        .unwrap();
    pending["schema"] = serde_json::json!(2);
    Store::open_unchecked(directory.path())
        .private(PENDING_TURN_FILE)
        .write_json(&pending)
        .unwrap();
    handle_hook(directory.path(), &serde_json::json!({"agent_bridge_claude_mod":1,"event":"turn.start","conversation":MOD_CONVERSATION,"data":start})).unwrap();
    let complete: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/249-complete.json")).unwrap();
    handle_hook(directory.path(), &serde_json::json!({"agent_bridge_claude_mod":1,"event":"turn.complete","conversation":MOD_CONVERSATION,"data":complete["event"]})).unwrap();
    let event = turn::wait(
        &Store::open_unchecked(directory.path()),
        0,
        Some(MOD_REQUEST),
        Some(&token),
        Instant::now() + Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .unwrap();
    assert_eq!(event.message, "PROBE-FINAL");
    assert_eq!(event.turn_id.as_deref(), Some(MOD_REQUEST));
    assert_eq!(
        event.provenance.as_deref(),
        Some("claude_mod_turn_complete")
    );
    assert_eq!(
        Reader::open_unchecked(directory.path())
            .events()
            .unwrap()
            .len(),
        1
    );
    assert!(
        turn::current_claim_token(&Reader::open_unchecked(directory.path()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn legacy_stop_draft_completion_remains_compatible() {
    for explicit in [false, true] {
        let (directory, _, _) = mod_regression_fixture(false);
        if explicit {
            Store::open_unchecked(directory.path())
                .private("claude-completion-mode.json")
                .write_json(&serde_json::json!({
                    "schema":1,"mode":"legacy_stop","reason":"platform_legacy","claude_version":null
                }))
                .unwrap();
        }
        let stop: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/249-stop-draft.json")).unwrap();
        handle_hook(directory.path(), &stop["event"]).unwrap();
        let paths = Reader::open_unchecked(directory.path()).events().unwrap();
        assert_eq!(paths.len(), 1);
        let event: crate::native::session::schema::SessionEvent =
            RecordReader::at(&paths[0]).json().unwrap();
        assert_eq!(event.message, "PROBE-DRAFT");
        assert_eq!(event.turn_id.as_deref(), Some(MOD_REQUEST));
        assert_eq!(event.provenance, None);
    }
}

#[test]
fn input_admission_refuses_before_messaging_and_journals_not_sent() {
    use crate::native::delivery::{Outcome, TurnKind};

    for native in [false, true] {
        for kind in [TurnKind::Initial, TurnKind::FollowUp] {
            let directory = tempfile::Builder::new()
                .prefix("session-")
                .tempdir()
                .unwrap();
            let directory = directory.path();
            launch_preflight(directory);
            std::fs::create_dir(directory.join("events")).unwrap();
            if native {
                completion::prepare(directory, false).unwrap();
            }
            let initial = kind == TurnKind::Initial;
            let state = if initial {
                SessionState::Working
            } else {
                SessionState::Ready
            };
            crate::native::session::update_status(directory, state, None, None).unwrap();
            let store = Store::open_unchecked(directory);
            let mut claim = if initial {
                turn::claim(&store, &[]).unwrap()
            } else {
                turn::claim_ready(
                    &store,
                    directory.file_name().unwrap().to_str().unwrap(),
                    &[],
                )
                .unwrap()
                .0
            };
            claim.begin_delivery().unwrap();
            let token = claim.token().to_owned();
            let surface = serde_json::from_value(serde_json::json!({
                "terminal":"windows-console","session_id":"unused-surface"
            }))
            .unwrap();
            let mut context = super::super::transport_test_context(directory, &surface, kind);
            context.claim_token = &token;
            context.deadline = Instant::now();
            context.provider_path = Path::new("must-not-start-a-messenger");
            let prepared = ClaudeMessage.prepare(&context).unwrap();
            let Outcome::NotSent(error) = ClaudeMessage.send(&context, &prepared, None) else {
                panic!("unready or legacy sessions must refuse before input");
            };
            let message = error.to_string();
            assert!(
                message.contains(if native {
                    "readiness was not observed"
                } else if cfg!(windows) {
                    "close and reopen"
                } else {
                    "fresh ask"
                }),
                "{message}"
            );
            assert!(!directory.join(PENDING_TURN_FILE).exists());
            assert!(!directory.read_dir().unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("claude-message-")
            }));
            claim
                .settle_delivery(turn::Delivery::NotSent(&error))
                .unwrap();
            let events = store.events().unwrap();
            assert_eq!(events.len(), 1);
            let event: crate::native::session::schema::SessionEvent =
                RecordReader::at(&events[0]).json().unwrap();
            assert_eq!(event.provider, "claude");
            assert_eq!(event.provenance.as_deref(), Some("delivery_not_sent"));
            assert_eq!(event.error.as_deref(), Some(message.as_str()));
            assert!(event.message.is_empty());
            assert!(event.turn_id.is_none());
            assert!(event.provider_session_id.is_none());
            assert!(turn::current_claim_token(&store).unwrap().is_none());
            assert_eq!(
                store.status().unwrap().state,
                if initial {
                    SessionState::Failed
                } else {
                    SessionState::Ready
                }
            );
        }
    }
}

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
fn diagnostics_never_promote_configured_inbound_to_runtime_messaging() {
    use super::super::super::diagnostics::{Availability, Context};
    let directory = tempfile::tempdir().unwrap();
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"crossSessionInbound":"accept"}),
    ] {
        std::fs::write(
            directory.path().join("claude-settings.json"),
            settings.to_string(),
        )
        .unwrap();
        let checks = ADAPTER.diagnose(Context {
            directory: Some(directory.path()),
            manifest: None,
            executable: None,
            current_version: Some("2.1.280"),
            workspace: directory.path(),
            probe: true,
            deadline: Instant::now(),
            environment: &crate::native::consent::Environment::default(),
        });
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "claude_messaging")
                .unwrap()
                .availability,
            Availability::Unknown
        );
        assert!(!directory.path().join("claude-pending-turn.json").exists());
    }
}

// Doctor lists the other live holders of a resumed conversation from the same registry
// scan the delivery boundaries use, and says nothing for a session that resumed none.
#[cfg(windows)]
#[test]
fn doctor_reports_other_live_holders_of_a_resumed_conversation() {
    use super::super::super::diagnostics::{Availability, Context};
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("sessions");
    std::fs::create_dir(&registry).unwrap();
    override_session_registry_for_test(Some(registry.clone()));
    let directory = root.path().join("session-reopendoc1");
    std::fs::create_dir(&directory).unwrap();
    let diagnose = |directory: &Path| {
        ADAPTER.diagnose(Context {
            directory: Some(directory),
            manifest: None,
            executable: None,
            current_version: Some("2.1.281"),
            workspace: directory,
            probe: false,
            deadline: Instant::now(),
            environment: &crate::native::consent::Environment::default(),
        })
    };
    let holders_check = |directory: &Path| {
        diagnose(directory)
            .into_iter()
            .find(|check| check.id == "claude_resumed_conversation_holders")
    };
    // No manifest, then a manifest without resumed_from: no check at all.
    assert!(holders_check(&directory).is_none());
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::json!({"schema": 1, "id": "session-reopendoc1"}).to_string(),
    )
    .unwrap();
    assert!(holders_check(&directory).is_none());

    std::fs::write(
        directory.join("manifest.json"),
        serde_json::json!({
            "schema": 1,
            "id": "session-reopendoc1",
            "resumed_from": {
                "session": "session-reopendoc0",
                "provider_session_id": REOPEN_TEST_CONVERSATION,
                "event_id": "event-1-1.json",
            },
        })
        .to_string(),
    )
    .unwrap();
    let write_live_entry = |pid: u32, name: &str| {
        let identity = terminal::windows_process_identity(pid).unwrap();
        std::fs::write(
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
    };
    // The managed process (this test process) is registered alone.
    write_live_entry(std::process::id(), "session-reopendoc1");
    let check = holders_check(&directory).unwrap();
    assert_eq!(check.availability, Availability::Available);
    assert_eq!(
        check.reason_code,
        "claude_resumed_conversation_exclusive_now"
    );

    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["detail"],
        format!(
            "No other live Claude Code process is registered for resumed conversation {REOPEN_TEST_CONVERSATION} at this moment."
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION, "other_holders": []})
    );

    // A foreign live resume is listed by pid.
    let mut foreign = std::process::Command::new("cmd.exe")
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    write_live_entry(foreign.id(), "foreign-resume");
    let check = holders_check(&directory).unwrap();
    let _ = foreign.kill();
    let _ = foreign.wait();
    assert_eq!(check.availability, Availability::Unavailable);
    assert_eq!(check.reason_code, "claude_resumed_conversation_shared");
    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["evidence"]["other_holders"],
        serde_json::json!([foreign.id()])
    );
    assert!(
        rendered["detail"]
            .as_str()
            .unwrap()
            .contains("gate reopen-conflict"),
        "{rendered}"
    );

    assert_eq!(
        rendered["detail"],
        format!(
            "Resumed conversation {REOPEN_TEST_CONVERSATION} is also held by live Claude Code process(es) {}; the next tell to this session will be refused with gate reopen-conflict while they stay registered.",
            foreign.id()
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION, "other_holders": [foreign.id()]})
    );

    // A scan that cannot complete is reported as unknown, never as exclusive.
    std::fs::remove_file(registry.join(format!("{}.json", std::process::id()))).unwrap();
    let check = holders_check(&directory).unwrap();
    assert_eq!(check.availability, Availability::Unknown);
    assert_eq!(check.reason_code, "claude_resumed_conversation_unverified");
    let rendered = serde_json::to_value(&check).unwrap();
    assert_eq!(
        rendered["detail"],
        format!(
            "The registry scan for resumed conversation {REOPEN_TEST_CONVERSATION} failed: reopened Claude session session-reopendoc1 is no longer registered for conversation {REOPEN_TEST_CONVERSATION} under {}; the next tell to this session will be refused with gate reopen-verification-failed until it succeeds.",
            registry.display()
        )
    );
    assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
    assert_eq!(
        rendered["evidence"],
        serde_json::json!({"provider_session_id": REOPEN_TEST_CONVERSATION})
    );
    override_session_registry_for_test(None);
}

#[test]
fn managed_launch_name_is_the_known_unique_session_id() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    launch_preflight(&directory);
    let plan = ADAPTER
        .prepare_launch(LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: root.path(),
            title: "Human title",
            prompt: "review this",
            yolo: false,
            model: None,
            effort: None,
        })
        .unwrap();

    assert!(
        plan.arguments
            .windows(2)
            .any(|pair| pair == ["--name", "session-safe123"])
    );
    assert!(
        !plan
            .arguments
            .iter()
            .any(|argument| argument == "Human title")
    );
}

#[test]
fn probe_and_version_queries_drop_the_same_markers_as_managed_launches() {
    use super::super::{FirstPartyCli, probe_environment_removals};
    assert_eq!(
        probe_environment_removals(FirstPartyCli::Claude),
        CLAUDE_CODE_SESSION_MARKERS
    );
    for marker in [
        "AI_AGENT",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CODEX_COMPANION_SESSION_ID",
        "CODEX_COMPANION_TRANSCRIPT_PATH",
    ] {
        assert!(probe_environment_removals(FirstPartyCli::Claude).contains(&marker));
    }
    for other in [FirstPartyCli::Codex, FirstPartyCli::Agy, FirstPartyCli::Pi] {
        assert!(probe_environment_removals(other).is_empty(), "{other:?}");
    }
}

#[test]
fn managed_launch_drops_inherited_claude_code_session_markers() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    std::fs::create_dir(&directory).unwrap();
    for windows in [false, true] {
        let directory = directory.join(if windows {
            "session-windows"
        } else {
            "session-macos"
        });
        std::fs::create_dir(&directory).unwrap();
        launch_preflight(&directory);
        let plan = prepare_launch_for_platform(
            LaunchContext {
                bridge_executable: Path::new("/opt/agent-bridge"),
                directory: &directory,
                workspace: root.path(),
                title: "Human title",
                prompt: "review this",
                yolo: false,
                model: None,
                effort: None,
            },
            windows,
        )
        .unwrap();
        // The decisive marker: an inherited CLAUDE_CODE_CHILD_SESSION stops the managed
        // session from registering its cross-session inbox (issue #42).
        assert!(
            plan.environment_removals
                .contains(&"CLAUDE_CODE_CHILD_SESSION")
        );
        for marker in [
            "AI_AGENT",
            "CLAUDE_CODE_BRIDGE_SESSION_ID",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_PID",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_EFFORT",
            // The caller's Claude Code Codex plugin names the caller's session (#149).
            "CODEX_COMPANION_SESSION_ID",
            "CODEX_COMPANION_TRANSCRIPT_PATH",
        ] {
            assert!(plan.environment_removals.contains(&marker), "{marker}");
        }
        // User configuration is not the adapter's to strip.
        for kept in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_MODEL",
            "CLAUDE_CONFIG_DIR",
            "PATH",
            "HOME",
        ] {
            assert!(!plan.environment_removals.contains(&kept), "{kept}");
        }
    }
}

#[test]
fn session_settings_enable_inbound_without_classic_completion_hooks() {
    assert_eq!(
        completion::settings(Path::new("unused-bridge")),
        serde_json::json!({"crossSessionInbound":"accept"})
    );
}

#[test]
fn every_platform_uses_the_official_cross_session_path_without_resume_fallback() {
    assert_eq!(
        ADAPTER.follow_up_transport().facts().name,
        "provider-cross-session-message"
    );
    assert_eq!(
        ADAPTER.initial_transport().facts().name,
        "provider-cross-session-message"
    );
    assert_eq!(
        ClaudeMessage.facts().readiness,
        Readiness::AfterLaunch {
            ready_delay: Duration::from_secs(2)
        }
    );
}

#[test]
fn windows_launch_keeps_large_initial_prompts_out_of_argv() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-large123");
    std::fs::create_dir(&directory).unwrap();
    launch_preflight(&directory);
    let prompt = "x".repeat(64 * 1024);
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: &directory,
            title: "large prompt",
            prompt: &prompt,
            yolo: false,
            model: None,
            effort: None,
        },
        true,
    )
    .unwrap();

    assert_eq!(
        plan.initial_prompt,
        InitialPromptDisposition::AwaitingDelivery
    );
    assert!(
        plan.arguments
            .iter()
            .all(|argument| !argument.to_string_lossy().contains(&prompt))
    );
}

#[test]
fn legacy_pending_turn_writer_preserves_schema_one_json() {
    let directory = tempfile::tempdir().unwrap();
    let _pending_file = ClaudeTurn::new("claude-turn-safe123")
        .unwrap()
        .install(&Store::open_unchecked(directory.path()))
        .unwrap();
    let written: serde_json::Value = Reader::open_unchecked(directory.path())
        .private(PENDING_TURN_FILE)
        .json()
        .unwrap();
    assert_eq!(
        written,
        serde_json::json!({
            "schema": 1,
            "request_id": "claude-turn-safe123",
            "marker": "<!-- agent-bridge-claude-turn:claude-turn-safe123 -->",
        })
    );
}

#[test]
fn pending_turn_ids_require_the_bounded_ascii_claude_prefix() {
    let maximum = format!("claude-turn-{}", "a".repeat(128 - "claude-turn-".len()));
    for valid in ["claude-turn-", "claude-turn-Ab09-1", maximum.as_str()] {
        assert!(ClaudeTurn::new(valid).is_ok(), "{valid:?}");
    }
    for invalid in [
        "",
        "claude-turn",
        "other-turn-safe123",
        "claude-turn-safe_123",
        "claude-turn-safe/123",
        "claude-turn-safe 123",
        "claude-turn-safe\n123",
        "claude-turn-safe\0",
        "claude-turn-한글",
        &format!("{maximum}a"),
    ] {
        assert!(ClaudeTurn::new(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn correlated_response_requires_the_exact_final_turn_marker() {
    let pending = super::turn::ClaudeTurn::new("claude-turn-safe123").unwrap();
    let exact = format!("completed response\n\n{}", pending.marker());
    assert_eq!(
        pending.correlated_response(&exact).unwrap(),
        "completed response"
    );
    assert!(pending.correlated_response("completed response").is_err());
    assert!(
        pending
            .correlated_response("completed response\n<!-- agent-bridge-claude-turn:other -->")
            .is_err()
    );
}

#[test]
fn initial_stop_owns_the_official_claude_hook_payload_schema() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "initial result",
    });

    handle_hook(directory.path(), &payload).unwrap();

    let events = crate::native::session::primitives::event_paths(directory.path()).unwrap();
    let event: crate::native::session::schema::SessionEvent =
        crate::native::session::primitives::read_json(&events[0]).unwrap();
    assert_eq!(event.message, "initial result");
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some("claude-session-id")
    );
    assert_eq!(event.turn_id, None);
}

#[test]
fn delayed_uncorrelated_stop_cannot_complete_a_later_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let first_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    first_claim.retain();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "initial result",
    });
    handle_hook(directory.path(), &payload).unwrap();

    let later_claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    later_claim.retain();
    crate::native::session::update_status(directory.path(), SessionState::Claimed, None, None)
        .unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    handle_hook(directory.path(), &payload).unwrap();

    assert_eq!(
        crate::native::session::primitives::event_paths(directory.path())
            .unwrap()
            .len(),
        1
    );
    assert!(
        directory
            .path()
            .join(crate::native::session::test_support::TURN_CLAIM_FILE)
            .exists()
    );
}

#[test]
fn correlated_stop_records_only_the_expected_claude_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let legacy_json = br#"{"schema":1,"request_id":"claude-turn-safe123","marker":"<!-- agent-bridge-claude-turn:claude-turn-safe123 -->"}"#;
    crate::native::session::primitives::write_private(
        &directory.path().join(PENDING_TURN_FILE),
        legacy_json,
    )
    .unwrap();
    let pending: ClaudeTurn = Reader::open_unchecked(directory.path())
        .private(PENDING_TURN_FILE)
        .json()
        .unwrap();
    assert!(serde_json::to_value(&pending).unwrap()["claim_token"].is_null());
    assert!(pending.expected_message().is_none());
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": format!("correlated result\n\n{}", pending.marker()),
    });

    handle_hook(directory.path(), &payload).unwrap();

    let events = crate::native::session::primitives::event_paths(directory.path()).unwrap();
    assert_eq!(events.len(), 1);
    let event: crate::native::session::schema::SessionEvent =
        crate::native::session::primitives::read_json(&events[0]).unwrap();
    assert_eq!(event.message, "correlated result");
    assert_eq!(
        event.provider_session_id.as_deref(),
        Some("claude-session-id")
    );
    assert_eq!(event.turn_id.as_deref(), Some("claude-turn-safe123"));
    assert!(directory.path().join("claude-pending-turn.json").is_file());
    assert!(
        !directory
            .path()
            .join(crate::native::session::test_support::TURN_CLAIM_FILE)
            .exists()
    );
}

#[test]
fn legacy_pending_reader_rejects_changed_schema_marker_and_invalid_id() {
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("request_id", serde_json::json!("claude-turn-invalid_id")),
        (
            "marker",
            serde_json::json!("<!-- agent-bridge-claude-turn:claude-turn-other -->"),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        std::fs::create_dir(directory.path().join("events")).unwrap();
        crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
            .unwrap();
        let claim = turn::claim(&store, &[]).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        let mut pending = serde_json::json!({
            "schema": 1,
            "request_id": "claude-turn-safe123",
            "marker": "<!-- agent-bridge-claude-turn:claude-turn-safe123 -->",
        });
        pending[field] = value;
        if field == "request_id" {
            pending["marker"] =
                serde_json::json!("<!-- agent-bridge-claude-turn:claude-turn-invalid_id -->");
        }
        store
            .private(PENDING_TURN_FILE)
            .write_json(&pending)
            .unwrap();
        let payload = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "claude-session-id",
            "last_assistant_message": "result\n<!-- agent-bridge-claude-turn:claude-turn-safe123 -->",
        });

        let error = handle_hook(directory.path(), &payload).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid Claude turn correlation state"),
            "{field}: {error:#}"
        );
        let reader = Reader::open_unchecked(directory.path());
        assert!(reader.events().unwrap().is_empty(), "{field}");
        assert_eq!(
            turn::current_claim_token(&reader).unwrap().as_deref(),
            Some(token.as_str()),
            "{field}"
        );
        assert_eq!(
            reader
                .private(PENDING_TURN_FILE)
                .json::<serde_json::Value>()
                .unwrap(),
            pending,
            "{field}"
        );
    }
}

#[test]
fn unmarked_claude_stop_keeps_the_pending_turn_uncommitted() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let pending = super::turn::ClaudeTurn::new("claude-turn-safe123").unwrap();
    crate::native::session::primitives::write_private(
        &directory.path().join("claude-pending-turn.json"),
        &serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "claude-session-id",
        "last_assistant_message": "uncorrelated result",
    });

    handle_hook(directory.path(), &payload).unwrap();

    assert!(
        crate::native::session::primitives::event_paths(directory.path())
            .unwrap()
            .is_empty()
    );
    assert!(directory.path().join("claude-pending-turn.json").is_file());
    assert!(
        directory
            .path()
            .join(crate::native::session::test_support::TURN_CLAIM_FILE)
            .is_file()
    );
}

#[test]
fn stop_failure_does_not_claim_an_uncorrelated_pending_turn() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("events")).unwrap();
    crate::native::session::update_status(directory.path(), SessionState::Working, None, None)
        .unwrap();
    let claim = turn::claim(&Store::open_unchecked(directory.path()), &[]).unwrap();
    claim.retain();
    let pending = super::turn::ClaudeTurn::new("claude-turn-safe123").unwrap();
    crate::native::session::primitives::write_private(
        &directory.path().join("claude-pending-turn.json"),
        &serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "StopFailure",
        "session_id": "claude-session-id",
        "error": "rate_limit",
        "error_details": "429 Too Many Requests",
    });

    handle_hook(directory.path(), &payload).unwrap();

    assert!(
        crate::native::session::primitives::event_paths(directory.path())
            .unwrap()
            .is_empty()
    );
    assert!(directory.path().join("claude-pending-turn.json").is_file());
    assert!(
        directory
            .path()
            .join(crate::native::session::test_support::TURN_CLAIM_FILE)
            .is_file()
    );
}

pub(super) const REOPEN_TEST_CONVERSATION: &str = "6928ca1c-1234-4abc-8def-0123456789ab";

#[test]
fn resume_plan_uses_the_official_resume_with_private_settings_and_managed_name_only() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session-reopenplan");
    std::fs::create_dir(&session).unwrap();
    launch_preflight(&session);
    let plan = prepare_resume_for_platform(
        ResumeContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &session,
            provider_session_id: REOPEN_TEST_CONVERSATION,
            yolo: false,
            model: None,
            effort: None,
        },
        true,
    )
    .unwrap();
    let settings_path = session.join("claude-settings.json");
    assert_eq!(
        plan.arguments,
        vec![
            OsString::from("--resume"),
            OsString::from(REOPEN_TEST_CONVERSATION),
            OsString::from("--settings"),
            settings_path.clone().into_os_string(),
            OsString::from("--name"),
            OsString::from("session-reopenplan"),
            OsString::from("--plugin-dir"),
            session.join("claude-mod").into_os_string(),
        ]
    );
    assert_no_op_watcher(plan.watcher);
    assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
    for forbidden in [
        "--dangerously-skip-permissions",
        "--model",
        "--effort",
        "--fork-session",
        "--session-id",
        "--print",
    ] {
        assert!(
            !plan.arguments.iter().any(|argument| argument == forbidden),
            "{forbidden} in resume plan"
        );
    }
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["crossSessionInbound"], "accept");
    assert!(settings.get("hooks").is_none());
}

#[test]
fn resume_is_refused_off_native_windows_and_for_non_uuid_identity() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session-reopenplan2");
    std::fs::create_dir(&session).unwrap();
    let error = prepare_resume_for_platform(
        ResumeContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &session,
            provider_session_id: REOPEN_TEST_CONVERSATION,
            yolo: false,
            model: None,
            effort: None,
        },
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with(
            "reopen unsupported: Claude reopen is implemented only for native Windows"
        ),
        "{error}"
    );
    assert!(!session.join("claude-settings.json").exists());
    assert_eq!(
        verify_reopen_available_for_platform(REOPEN_TEST_CONVERSATION, false)
            .unwrap_err()
            .to_string(),
        error.to_string()
    );
    for bad in [
        "",
        "not-a-uuid",
        "6928ca1c-1234-4abc-8def-0123456789ab-extra",
        "6928ca1c_1234_4abc_8def_0123456789ab",
        "../../../../../../../../../../etc/pw",
    ] {
        let error = verify_reopen_available_for_platform(bad, true).unwrap_err();
        assert!(
            error.to_string().contains("not a Claude session UUID"),
            "{bad:?}: {error}"
        );
    }
    verify_reopen_available_for_platform(REOPEN_TEST_CONVERSATION, true).unwrap();
    verify_reopen_available_for_platform("6928CA1C-1234-4ABC-8DEF-0123456789AB", true).unwrap();
}

#[test]
fn reopen_bundle_exists_on_every_platform() {
    assert!(ADAPTER.reopen().is_ok());
}

#[test]
fn doctor_empty_registry_preserves_unverified_detail_and_omission() {
    use super::super::super::diagnostics::Availability;
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry");
    std::fs::create_dir(&registry).unwrap();
    override_session_registry_for_test(Some(registry.clone()));
    for name in ["session-doctorempty", "invalid"] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        assert!(resumed_conversation_holders_check(&directory).is_none());
        for manifest in [
            serde_json::json!({}),
            serde_json::json!({"resumed_from": {}}),
            serde_json::json!({"resumed_from": {"provider_session_id": 1}}),
        ] {
            std::fs::write(directory.join("manifest.json"), manifest.to_string()).unwrap();
            assert!(resumed_conversation_holders_check(&directory).is_none());
        }
        for conversation in [REOPEN_TEST_CONVERSATION, "not-a-uuid"] {
            std::fs::write(
                directory.join("manifest.json"),
                serde_json::json!({"resumed_from": {"provider_session_id": conversation}})
                    .to_string(),
            )
            .unwrap();
            let check = resumed_conversation_holders_check(&directory);
            if name == "invalid" {
                assert!(check.is_none());
                continue;
            }
            let check = check.unwrap();
            assert_eq!(check.id, "claude_resumed_conversation_holders");
            assert_eq!(check.availability, Availability::Unknown);
            assert_eq!(check.reason_code, "claude_resumed_conversation_unverified");
            let rendered = serde_json::to_value(&check).unwrap();
            assert_eq!(
                rendered["detail"],
                format!(
                    "The registry scan for resumed conversation {conversation} failed: reopened Claude session {name} is no longer registered for conversation {conversation} under {}; the next tell to this session will be refused with gate reopen-verification-failed until it succeeds.",
                    registry.display()
                )
            );
            assert_eq!(rendered["next_action"], DOCTOR_HOLDER_NEXT_ACTION);
            assert_eq!(
                rendered["evidence"],
                serde_json::json!({"provider_session_id": conversation})
            );
        }
    }
    override_session_registry_for_test(None);
}

const DOCTOR_HOLDER_NEXT_ACTION: &str = "Detection is best-effort and taken only at launch, before each delivery, and now; a foreign claude --resume can still interleave between checks. Close the other holder before the next tell, or accept the shared transcript.";

// The expected policy prefix of a Claude command, in the fixed order the adapter emits it:
// the bypass only when requested, then effort, then model.
fn claude_policy(yolo: bool, model: Option<&str>, effort: Option<&str>) -> Vec<OsString> {
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
#[test]
fn launch_plan_is_the_complete_command_for_every_policy_and_platform() {
    for windows in [false, true] {
        for yolo in [false, true] {
            for (model, mapped) in [(None, None), (Some("Fable5"), Some("Fable"))] {
                for effort in [None, Some("high")] {
                    let root = tempfile::tempdir().unwrap();
                    let directory = root.path().join(if windows {
                        "session-windows"
                    } else {
                        "session-macos"
                    });
                    std::fs::create_dir(&directory).unwrap();
                    launch_preflight(&directory);
                    let settings_path = directory.join("claude-settings.json");
                    let _ = std::fs::remove_file(&settings_path);
                    let plan = prepare_launch_for_platform(
                        LaunchContext {
                            bridge_executable: Path::new("/opt/agent-bridge"),
                            directory: &directory,
                            workspace: root.path(),
                            title: "Claude test",
                            prompt: "claude prompt",
                            yolo,
                            model,
                            effort,
                        },
                        windows,
                    )
                    .unwrap();
                    let label =
                        format!("windows={windows} yolo={yolo} model={model:?} effort={effort:?}");
                    let mut expected = claude_policy(yolo, mapped, effort);
                    expected.extend([
                        OsString::from("--settings"),
                        settings_path.clone().into_os_string(),
                        OsString::from("--name"),
                        OsString::from(if windows {
                            "session-windows"
                        } else {
                            "session-macos"
                        }),
                    ]);
                    expected.extend([
                        OsString::from("--plugin-dir"),
                        directory.join("claude-mod").into_os_string(),
                    ]);
                    assert_eq!(plan.arguments, expected, "{label}");
                    assert_eq!(
                        plan.initial_prompt,
                        InitialPromptDisposition::AwaitingDelivery,
                        "{label}"
                    );
                    assert_no_op_watcher(plan.watcher);
                    assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
                    let settings: serde_json::Value =
                        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap())
                            .unwrap();
                    assert_eq!(settings["crossSessionInbound"], "accept", "{label}");
                    assert!(settings.get("hooks").is_none(), "{label}");
                }
            }
        }
    }
}

// The complete resume command carries the new request's policy ahead of the official
// resume arguments and never a prompt; the reopened session awaits delivery.
#[test]
fn resume_plan_is_the_complete_command_for_every_policy() {
    for yolo in [false, true] {
        for (model, mapped) in [(None, None), (Some("Fable5"), Some("Fable"))] {
            for effort in [None, Some("max")] {
                let root = tempfile::tempdir().unwrap();
                let session = root.path().join("session-reopenplan");
                std::fs::create_dir(&session).unwrap();
                launch_preflight(&session);
                let settings_path = session.join("claude-settings.json");
                let plan = prepare_resume_for_platform(
                    ResumeContext {
                        bridge_executable: Path::new("/opt/agent-bridge"),
                        directory: &session,
                        provider_session_id: REOPEN_TEST_CONVERSATION,
                        yolo,
                        model,
                        effort,
                    },
                    true,
                )
                .unwrap();
                let label = format!("yolo={yolo} model={model:?} effort={effort:?}");
                let mut expected = claude_policy(yolo, mapped, effort);
                expected.extend([
                    OsString::from("--resume"),
                    OsString::from(REOPEN_TEST_CONVERSATION),
                    OsString::from("--settings"),
                    settings_path.clone().into_os_string(),
                    OsString::from("--name"),
                    OsString::from("session-reopenplan"),
                    OsString::from("--plugin-dir"),
                    session.join("claude-mod").into_os_string(),
                ]);
                assert_eq!(plan.arguments, expected, "{label}");
                assert_eq!(
                    plan.initial_prompt,
                    InitialPromptDisposition::AwaitingDelivery,
                    "{label}"
                );
                assert_eq!(plan.environment_removals, CLAUDE_CODE_SESSION_MARKERS);
            }
        }
    }
}
