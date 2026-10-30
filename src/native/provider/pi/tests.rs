use super::monitor::HOOK_FAILURE_FILE;
use super::startup_ready::{STARTUP_READY_FILE, StartupReady};
use super::test_support::{claim_pending_turn, marked};
use super::*;
use crate::native::session::primitives::{event_paths, read_json, write_json_atomic};
use crate::native::session::test_support::TURN_CLAIM_FILE;
use crate::native::session::update_status;
use crate::native::session::{SessionState, schema::SessionEvent};
use std::fs;

#[test]
fn declared_transports_preserve_framing_and_pending_cleanup() {
    let initial_transport = PiInitialPaste::new();
    let follow_up_transport = PiPaste::new();
    for facts in [initial_transport.facts(), follow_up_transport.facts()] {
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
    let pending = claim_pending_turn(directory.path());
    let mut initial =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::Initial);
    initial.claim_token = &pending.claim_token;
    let prepared = initial_transport.prepare(&initial).unwrap();
    let before = std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_initial_prompt(directory.path(), initial.prompt).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    initial_transport.discard_prepared(&initial, Some(&prepared));
    assert_eq!(
        std::fs::read(directory.path().join(PENDING_TURN_FILE)).unwrap(),
        before
    );
    let mut follow =
        super::super::transport_test_context(directory.path(), &surface, TurnKind::FollowUp);
    follow.claim_token = &pending.claim_token;
    let prepared = follow_up_transport.prepare(&follow).unwrap();
    let pending = read_pending_turn(directory.path()).unwrap().unwrap();
    assert_eq!(
        prepared.prompt,
        terminal_correlated_prompt(follow.prompt, &pending, cfg!(windows)).unwrap()
    );
    assert!(prepared.correlation_id.is_none());
    follow_up_transport.discard_prepared(&follow, Some(&prepared));
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
    follow_up_transport.prepare(&follow).unwrap();
    follow_up_transport.discard_prepared(&follow, None);
    assert!(!directory.path().join(PENDING_TURN_FILE).exists());
}

#[test]
fn workspace_trust_requires_exact_positive_but_respects_parent_decline() {
    use super::super::super::consent::{self, Trust};
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().canonicalize().unwrap();
    let env = consent::Environment::at_home(tmp.path());
    std::fs::create_dir_all(tmp.path().join(".pi/agent")).unwrap();
    let store = tmp.path().join(".pi/agent").join("trust.json");
    let key = consent::native_key(&workspace).unwrap();
    crate::native::session::primitives::write_json_atomic(
        &store,
        &serde_json::json!({key.clone():true}),
    )
    .unwrap();
    // The evidence names the store the adapter read, spelled as its home resolves it:
    // the `.pi/agent` component keeps its slash on every platform.
    let Trust::Trusted(evidence) = ADAPTER.workspace_trust(&workspace, &env, None).unwrap() else {
        panic!("trusted");
    };
    assert_eq!(evidence.provider, "pi");
    assert_eq!(evidence.key, key);
    assert_eq!(evidence.store, store);
    let serialized = serde_json::to_value(&evidence).unwrap()["store"]
        .as_str()
        .unwrap()
        .to_owned();
    if cfg!(windows) {
        assert!(
            serialized.ends_with(r"\.pi/agent\trust.json"),
            "{serialized}"
        );
    } else {
        assert!(
            serialized.ends_with("/.pi/agent/trust.json"),
            "{serialized}"
        );
    }
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &env, None)
            .unwrap(),
        Trust::Absent
    );
    crate::native::session::primitives::write_json_atomic(&store, &serde_json::json!({key:false}))
        .unwrap();
    assert_eq!(
        ADAPTER
            .workspace_trust(&workspace.join("child"), &env, None)
            .unwrap(),
        Trust::Declined
    );
    std::fs::write(&store, b"[]").unwrap();
    assert!(ADAPTER.workspace_trust(&workspace, &env, None).is_err());
}

#[test]
fn the_pi_home_is_trust_json_under_the_agent_dir_or_the_user_home() {
    use crate::native::consent::Environment;
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_str().unwrap();
    let environment = |agent_dir: Option<&str>| Environment {
        home: Some(home_str.into()),
        pi_coding_agent_dir: agent_dir.map(Into::into),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&environment(None)).unwrap().trust_store,
        home.path().join(".pi/agent").join("trust.json")
    );
    let custom = home.path().join("custom-pi");
    assert_eq!(
        Home::resolve(&environment(custom.to_str()))
            .unwrap()
            .trust_store,
        custom.join("trust.json")
    );
    // The override is kept as spelled: relative stays relative, empty names the caller's
    // working directory.
    assert_eq!(
        Home::resolve(&environment(Some("relative")))
            .unwrap()
            .trust_store,
        Path::new("relative").join("trust.json")
    );
    assert_eq!(
        Home::resolve(&environment(Some(""))).unwrap().trust_store,
        std::path::PathBuf::from("trust.json")
    );
    // The user home is required even with the override.
    let override_only = Environment {
        pi_coding_agent_dir: Some(custom.as_os_str().to_owned()),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&override_only).unwrap_err().to_string(),
        "no user home for workspace trust lookup"
    );
    let profile_only = Environment {
        userprofile: Some(home_str.into()),
        ..Default::default()
    };
    assert_eq!(
        Home::resolve(&profile_only).unwrap().trust_store,
        home.path().join(".pi/agent").join("trust.json")
    );
}

#[test]
fn diagnostics_describe_pi_owned_fallback_without_claiming_live_delivery() {
    use super::super::super::diagnostics::{Availability, Context};
    use super::NativeProviderAdapter;
    let checks = super::ADAPTER.diagnose(Context {
        directory: None,
        manifest: None,
        executable: None,
        current_version: None,
        workspace: std::path::Path::new("."),
        probe: false,
        deadline: std::time::Instant::now(),
        environment: &crate::native::consent::Environment::default(),
    });
    assert_eq!(checks[0].reason_code, "pi_terminal_fallback");
    assert_eq!(checks[0].availability, Availability::Unknown);
    assert_eq!(checks[1].id, "pi_provider_credentials");
    assert_eq!(checks[1].availability, Availability::Unknown);
}

fn run_extension_harness(directory: &Path, startup: bool) {
    let mut command = std::process::Command::new("node");
    command.arg(directory.join("test.mjs")).arg(directory);
    if startup {
        command.arg("startup");
    }
    let output = command
        .output()
        .expect("Node on PATH is required to execute the Pi extension tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cancel_extension_executes_lifecycle_and_partial_failure_cases_in_node() {
    use crate::native::session::schema::SessionManifest;
    use crate::native::session::{CoreRecord, cancel};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("extension.js"), bridge_extension()).unwrap();
    fs::write(root.join("test.mjs"), include_str!("../pi_cancel_test.mjs")).unwrap();
    let cases = [
        "matched_duplicate",
        "prompt_mismatch",
        "claim_mismatch",
        "claim_replaced",
        "stale_callback",
        "direct_escape",
        "write_failure",
        "abort_failure",
        "agent_settled",
        "session_shutdown",
        "normal_completion_wins",
        "two_claims_and_retry",
        "settlement_without_aborted_end",
        "retry_without_before_agent_start",
        "partial_json_then_valid_record",
        "tool_abort_accepted",
        "tool_abort_direct_escape",
        "tool_network_error",
        "tool_abort_message_not_exact",
        "tool_abort_wrong_accepted_claim",
        "normal_complete",
    ];
    let mut inputs = serde_json::Map::new();
    // Each case keeps its own live claim. Secondary claims supply mismatch/replacement bytes.
    for name in cases {
        for suffix in ["", "_second"] {
            let key = format!("{name}{suffix}");
            let dir = root.join(&key);
            fs::create_dir(&dir).unwrap();
            fs::create_dir(dir.join("events")).unwrap();
            update_status(&dir, SessionState::Working, None, None).unwrap();
            let store = Store::open_unchecked(&dir);
            store
                .write_manifest(&SessionManifest {
                    schema: 1,
                    id: key.clone(),
                    provider: "pi".into(),
                    provider_path: "pi".into(),
                    provider_version: "1.0.4".into(),
                    workspace: root.to_path_buf(),
                    title: key.clone(),
                    model: None,
                    effort: None,
                    yolo: false,
                    created_unix_ms: 1,
                })
                .unwrap();
            let pending = claim_pending_turn(&dir);
            let prompt = if name == "tool_abort_accepted" {
                terminal_correlated_prompt("prompt\nwith\ttabs", &pending, true).unwrap()
            } else {
                correlated_prompt("prompt", &pending)
            };
            inputs.insert(
                key,
                serde_json::json!([{
                    "token": pending.claim_token,
                    "claim": fs::read_to_string(dir.join(CoreRecord::TurnClaim.name())).unwrap(),
                    "prompt": prompt, "response": marked("result", &pending),
                }]),
            );
        }
    }
    write_json_atomic(&root.join("inputs.json"), &inputs).unwrap();
    // The extension itself writes schema-2 readiness before real cancel admission.
    run_extension_harness(root, true);
    for (key, records) in &mut inputs {
        let dir = root.join(key);
        assert!(matches!(
            ADAPTER.cancel_support(&dir).unwrap(),
            super::super::CancelSupport::Supported
        ));
        cancel::request(&Store::open_unchecked(&dir), key).unwrap();
        records[0]["cancel"] =
            serde_json::json!(fs::read_to_string(dir.join(CoreRecord::Cancel.name())).unwrap());
        fs::remove_file(dir.join(CoreRecord::Cancel.name())).unwrap();
    }
    for name in cases {
        let second = inputs[&format!("{name}_second")][0].clone();
        inputs
            .get_mut(name)
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(second);
    }
    write_json_atomic(&root.join("inputs.json"), &inputs).unwrap();
    run_extension_harness(root, false);
    let outputs: Vec<serde_json::Value> = read_json(&root.join("payloads.json")).unwrap();
    for (name, cancelled, error) in [
        ("normal_complete", false, None),
        (
            "matched_duplicate",
            true,
            Some("cancelled: Pi turn ended with aborted"),
        ),
        ("direct_escape", false, Some("Pi turn ended with aborted")),
        (
            "tool_abort_accepted",
            true,
            Some("cancelled: Pi turn ended with error: This operation was aborted"),
        ),
        ("write_failure", false, Some("Pi turn ended with aborted")),
        ("normal_completion_wins", false, None),
        ("prompt_mismatch", false, None),
    ] {
        let dir = root.join(name);
        let payloads = outputs.iter().find(|v| v["name"] == name).unwrap()["payloads"]
            .as_array()
            .unwrap();
        assert!(dir.join(CoreRecord::TurnClaim.name()).exists());
        if name == "prompt_mismatch" {
            assert!(payloads.is_empty());
            assert!(event_paths(&dir).unwrap().is_empty());
            assert_eq!(
                turn::current_claim_token(&Reader::open_unchecked(&dir))
                    .unwrap()
                    .unwrap(),
                inputs[name][0]["token"]
            );
            continue;
        }
        for payload in payloads {
            ADAPTER.handle_hook(&dir, payload).unwrap();
        }
        let paths = event_paths(&dir).unwrap();
        assert_eq!(paths.len(), 1, "{name}");
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.cancelled, cancelled, "{name}");
        assert_eq!(event.error.as_deref(), error, "{name}");
        assert_eq!(
            event.message,
            if error.is_none() { "result" } else { "" },
            "{name}"
        );
        assert!(!dir.join(CoreRecord::TurnClaim.name()).exists(), "{name}");
    }
    // Carry the JS-written receipt to a real next claim in that same session.
    let dir = root.join("matched_duplicate");
    let old: StartupReady = read_json(&dir.join(STARTUP_READY_FILE)).unwrap();
    let next = claim_pending_turn(&dir);
    let carried: StartupReady = read_json(&dir.join(STARTUP_READY_FILE)).unwrap();
    assert_eq!(carried.schema, old.schema);
    assert_eq!(carried.schema, 2);
    assert_eq!(carried.session_id, old.session_id);
    assert_ne!(carried.claim_token, old.claim_token);
    assert_eq!(carried.claim_token, next.claim_token);
    assert!(matches!(
        ADAPTER.cancel_support(&dir).unwrap(),
        super::super::CancelSupport::Supported
    ));
}

#[test]
fn cancel_support_requires_schema_two_and_current_claim() {
    let directory = tempfile::tempdir().unwrap();
    let pending = claim_pending_turn(directory.path());
    for (schema, token, session, supported) in [
        (1, pending.claim_token.as_str(), "native", false),
        (2, "1-2-3", "native", false),
        (2, pending.claim_token.as_str(), "", false),
        (3, pending.claim_token.as_str(), "native", false),
        (2, pending.claim_token.as_str(), "native", true),
    ] {
        write_json_atomic(
            &directory.path().join(STARTUP_READY_FILE),
            &StartupReady {
                input_ack_schema: None,
                schema,
                claim_token: token.into(),
                session_id: session.into(),
            },
        )
        .unwrap();
        assert_eq!(
            matches!(
                ADAPTER.cancel_support(directory.path()).unwrap(),
                super::super::CancelSupport::Supported
            ),
            supported
        );
    }
    for contents in [None, Some("not-json")] {
        fs::remove_file(directory.path().join(STARTUP_READY_FILE)).unwrap();
        if let Some(contents) = contents {
            fs::write(directory.path().join(STARTUP_READY_FILE), contents).unwrap();
        }
        assert!(matches!(
            ADAPTER.cancel_support(directory.path()).unwrap(),
            super::super::CancelSupport::Unsupported(_)
        ));
        // Restore for the next removal.
        fs::write(directory.path().join(STARTUP_READY_FILE), "{}").unwrap();
    }
}

#[test]
fn cancel_support_refuses_a_legacy_record_before_reading_the_claim() {
    // A schema 1 record never offers cancel, so an unreadable Claim is not consulted:
    // the answer stays Unsupported, as it was before the shared reader. A schema 2 record
    // does consult the Claim, and its read failure surfaces as before.
    let directory = tempfile::tempdir().unwrap();
    let pending = claim_pending_turn(directory.path());
    let claim = directory.path().join("turn.claim");
    fs::remove_file(&claim).unwrap();
    fs::create_dir(&claim).unwrap();
    for (schema, unsupported) in [(1, true), (2, false)] {
        write_json_atomic(
            &directory.path().join(STARTUP_READY_FILE),
            &StartupReady {
                input_ack_schema: None,
                schema,
                claim_token: pending.claim_token.clone(),
                session_id: "native".into(),
            },
        )
        .unwrap();
        let support = ADAPTER.cancel_support(directory.path());
        let unsupported_now = matches!(support, Ok(super::super::CancelSupport::Unsupported(_)));
        assert_eq!(unsupported_now, unsupported, "schema {schema}");
    }
}

#[test]
fn cancel_accepted_cleanup_is_owned_by_pending_turn_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let accepted = directory.path().join("pi-cancel-accepted.json");
    let first = claim_pending_turn(directory.path());
    write_json_atomic(
        &accepted,
        &serde_json::json!({"schema":1,"claim_token":first.claim_token}),
    )
    .unwrap();
    store
        .record(crate::native::session::CoreRecord::TurnClaim)
        .remove()
        .unwrap();
    let next = claim_pending_turn(directory.path());
    assert!(!accepted.exists());
    write_json_atomic(
        &accepted,
        &serde_json::json!({"schema":1,"claim_token":next.claim_token}),
    )
    .unwrap();
    cancel_pending_turn(directory.path(), &first.claim_token).unwrap();
    assert!(
        accepted.exists(),
        "stale rollback cannot remove the new claim's record"
    );
    cancel_pending_turn(directory.path(), &next.claim_token).unwrap();
    assert!(!accepted.exists());
}

#[test]
fn cancel_support_survives_follow_up_and_delivery_rollback_without_upgrading_legacy() {
    for schema in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let pending = claim_pending_turn(directory.path());
        write_json_atomic(
            &directory.path().join(STARTUP_READY_FILE),
            &StartupReady {
                input_ack_schema: None,
                schema,
                claim_token: pending.claim_token,
                session_id: "native".into(),
            },
        )
        .unwrap();
        for token in ["4-5-6", "7-8-9"] {
            fs::write(directory.path().join(TURN_CLAIM_FILE), token).unwrap();
            install_pending_turn(directory.path(), token).unwrap();
            assert_eq!(
                matches!(
                    ADAPTER.cancel_support(directory.path()).unwrap(),
                    super::super::CancelSupport::Supported
                ),
                schema == 2
            );
            cancel_pending_turn(directory.path(), token).unwrap();
        }
    }
}

#[test]
fn windows_console_prompt_preserves_multiline_input_without_raw_submission_keys() {
    let pending = PendingPiTurn::new("1-2-3").unwrap();
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

fn assert_core_record_contract(extension: &str) {
    use crate::native::session::CoreRecord;
    fn allowed(record: CoreRecord) -> bool {
        match record {
            CoreRecord::Hold => false,
            CoreRecord::Cancel => true,
            CoreRecord::TurnClaim => true,
            CoreRecord::Completion => false,
            CoreRecord::Owner => false,
            CoreRecord::Closed => false,
            CoreRecord::Terminal => false,
            CoreRecord::TerminalClosing => false,
            CoreRecord::TerminalClosed => false,
            #[cfg(target_os = "macos")]
            CoreRecord::TerminalCloseIntent => false,
            CoreRecord::LegacyResumePending => false,
            CoreRecord::LegacyResumeRunning => false,
            CoreRecord::ReopenMarker => false,
            CoreRecord::ReopenRefusal => false,
            CoreRecord::ProviderProcess => false,
            CoreRecord::Status => false,
            CoreRecord::Manifest => false,
            CoreRecord::InitialPrompt => false,
            CoreRecord::Events => false,
            CoreRecord::Requests => false,
            CoreRecord::Launch => false,
        }
    }
    let records = [
        CoreRecord::Hold,
        CoreRecord::Cancel,
        CoreRecord::TurnClaim,
        CoreRecord::Completion,
        CoreRecord::Owner,
        CoreRecord::Closed,
        CoreRecord::Terminal,
        CoreRecord::TerminalClosing,
        CoreRecord::TerminalClosed,
        #[cfg(target_os = "macos")]
        CoreRecord::TerminalCloseIntent,
        CoreRecord::LegacyResumePending,
        CoreRecord::LegacyResumeRunning,
        CoreRecord::ReopenMarker,
        CoreRecord::ReopenRefusal,
        CoreRecord::ProviderProcess,
        CoreRecord::Status,
        CoreRecord::Manifest,
        CoreRecord::InitialPrompt,
        CoreRecord::Events,
        CoreRecord::Requests,
        CoreRecord::Launch,
    ];
    for record in records {
        assert_eq!(
            extension.contains(record.name()),
            allowed(record),
            "Pi core record contract changed for {}; update the declared contract",
            record.name()
        );
    }
}

#[test]
fn extension_reports_only_settled_results_without_changing_tool_policy() {
    let extension = bridge_extension();

    assert!(extension.contains("agent_start"));
    assert!(extension.contains("before_agent_start"));
    assert!(extension.contains("agent_end"));
    assert!(extension.contains("agent_settled"));
    assert!(extension.contains("stopReason"));
    assert!(extension.contains("agent_bridge_error"));
    assert!(extension.contains("agent_bridge_claim_token"));
    assert!(extension.contains("agent_bridge_prompt_correlated"));
    assert!(extension.contains("event.prompt.includes"));
    assert_core_record_contract(extension);
    assert!(extension.contains(CANCEL_ACCEPTED_FILE));
    assert!(extension.contains(STARTUP_READY_FILE));
    assert!(extension.contains(HOOK_FAILURE_FILE));
    assert!(extension.contains("renameSync"));
    assert!(extension.contains("native-hook\", \"pi"));
    assert!(extension.contains("timeout: 10000"));
    assert!(!extension.contains("tool_call"));
    assert!(!extension.contains("--approve"));
}

#[test]
fn reopen_is_refused_with_the_adapters_own_reason() {
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), PI_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Pi"),
        "{error}"
    );
    let directory = tempfile::tempdir().unwrap();
    let error = ADAPTER.reopen().err().expect("unsupported provider");
    assert_eq!(error.to_string(), PI_REOPEN_UNSUPPORTED);
    assert!(
        error.to_string().starts_with("reopen unsupported: Pi"),
        "{error}"
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

// The expected policy prefix of a Pi command, in the fixed order the adapter emits it:
// `--approve` only when the bypass is requested, then thinking, then model.
fn pi_policy(yolo: bool, model: Option<&str>, effort: Option<&str>) -> Vec<OsString> {
    let mut expected = Vec::new();
    if yolo {
        expected.push(OsString::from("--approve"));
    }
    if let Some(effort) = effort {
        expected.extend([OsString::from("--thinking"), OsString::from(effort)]);
    }
    if let Some(model) = model {
        expected.extend([OsString::from("--model"), OsString::from(model)]);
    }
    expected
}

// Pi selects the Codex caller markers for its probes and launches, and nothing else.
#[test]
fn probes_drop_exactly_the_codex_caller_markers() {
    use crate::native::process_env::tests::{CODEX_CALLER_MARKERS, CODEX_CONTEXT_KEPT};
    assert_eq!(
        ADAPTER.probe_environment_removals(),
        &CODEX_CALLER_MARKERS[..]
    );
    for kept in CODEX_CONTEXT_KEPT {
        assert!(
            !ADAPTER.probe_environment_removals().contains(&kept),
            "{kept}"
        );
    }
}

// The complete launch command for every policy combination, both platform branches and
// both consent answers: policy first, the result extension, the managed name, a second
// `--approve` when consent authorizes the workspace (both stay when both apply), and off
// Windows the correlated prompt last.
#[test]
fn launch_plan_is_the_complete_command_for_every_policy_platform_and_consent() {
    for consent in [false, true] {
        for windows in [false, true] {
            for yolo in [false, true] {
                for (model, mapped) in [
                    (None, None),
                    (Some("Fable"), Some("anthropic/claude-fable-5")),
                ] {
                    for effort in [None, Some("minimal")] {
                        let root = tempfile::tempdir().unwrap();
                        let directory = root.path().join("session-safe123");
                        fs::create_dir(&directory).unwrap();
                        fs::create_dir(directory.join("events")).unwrap();
                        let workspace = root.path().join("workspace");
                        fs::create_dir(&workspace).unwrap();
                        let workspace = workspace.canonicalize().unwrap();
                        turn::claim(&Store::open_unchecked(&directory), &[])
                            .unwrap()
                            .retain();
                        super::test_support::launching_status(&directory);
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
                                bridge_executable: Path::new("/opt/agent-bridge"),
                                directory: &directory,
                                workspace: &workspace,
                                title: "Pi test",
                                prompt: "pi prompt",
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
                        let extension_path = directory.join("pi-agent-bridge.js");
                        let mut expected = pi_policy(yolo, mapped, effort);
                        expected.extend([
                            OsString::from("--extension"),
                            extension_path.clone().into_os_string(),
                            OsString::from("--name"),
                            OsString::from("Pi test"),
                        ]);
                        if consent {
                            expected.push(OsString::from("--approve"));
                        }
                        if !windows {
                            let framed = correlated_prompt("pi prompt", &pending);
                            assert!(framed.starts_with("pi prompt"), "{label}");
                            assert!(
                                framed.contains("[Agent Bridge Pi turn protocol]"),
                                "{label}"
                            );
                            assert!(framed.ends_with(&pending.marker), "{label}");
                            expected.push(OsString::from(framed));
                        }
                        assert_eq!(plan.arguments, expected, "{label}");
                        assert_eq!(
                            plan.arguments
                                .iter()
                                .filter(|argument| *argument == "--approve")
                                .count(),
                            usize::from(yolo) + usize::from(consent),
                            "{label}"
                        );
                        assert!(
                            !plan
                                .arguments
                                .iter()
                                .any(|argument| argument.to_string_lossy().contains("dangerously")),
                            "{label}"
                        );
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
                            format!("{:?}", PiHookFailureWatcher),
                            "{label}"
                        );
                        assert_eq!(
                            plan.environment_removals,
                            &crate::native::process_env::tests::CODEX_CALLER_MARKERS[..],
                            "{label}"
                        );
                        let extension = fs::read_to_string(&extension_path).unwrap();
                        assert!(extension.contains("agent_settled"), "{label}");
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

// The plan carries the hook-failure watcher unstarted: a recovery signal written before
// the plan is built is still there after it, and only `start` hands the directory to the
// monitor, which consumes the signal before `stop` returns.
#[test]
fn launch_plan_watcher_starts_the_hook_failure_monitor_only_when_started() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-safe123");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    super::test_support::launching_status(&directory);
    let signal = directory.join(HOOK_FAILURE_FILE);
    fs::write(
        &signal,
        r#"{"error":"native hook exited with status 1","claim_token":"0-0-0"}"#,
    )
    .unwrap();
    let plan = prepare_launch_for_platform(
        LaunchContext {
            bridge_executable: Path::new("/opt/agent-bridge"),
            directory: &directory,
            workspace: root.path(),
            title: "Pi test",
            prompt: "pi prompt",
            yolo: false,
            model: None,
            effort: None,
        },
        false,
        crate::native::consent::authorized,
    )
    .unwrap();
    assert_eq!(
        format!("{:?}", plan.watcher),
        format!("{:?}", PiHookFailureWatcher)
    );
    assert!(signal.exists(), "building the plan consumed the signal");

    let active = plan.watcher.start(&directory).unwrap();
    active.stop().unwrap();
    assert!(
        !signal.exists(),
        "the started monitor did not consume the signal"
    );
}

#[test]
fn usage_extension_reports_pi_usage_beside_every_outcome_in_node() {
    use crate::native::session::schema::SessionManifest;
    use crate::native::session::{CoreRecord, cancel};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("extension.js"), bridge_extension()).unwrap();
    fs::write(root.join("test.mjs"), include_str!("../pi_usage_test.mjs")).unwrap();
    fs::write(
        root.join("fixtures.json"),
        include_str!("../pi_usage_fixtures.json"),
    )
    .unwrap();
    let cases = [
        "one_run_two_messages",
        "two_runs",
        "retry_without_before_agent_start",
        "reasoning_omitted_unless_everywhere",
        "missing_usage",
        "fractional_operand",
        "negative_operand",
        "unsafe_operand",
        "unsafe_sum",
        "fractional_reasoning",
        "error_run_is_uncertain",
        "cancelled_keeps_usage",
        "claim_replaced_resets",
        "delivery_failure_persists_usage",
        "delivery_failure_persists_usage_error",
        "session_shutdown_resets",
    ];
    let mut inputs = serde_json::Map::new();
    for name in cases {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("events")).unwrap();
        update_status(&dir, SessionState::Working, None, None).unwrap();
        Store::open_unchecked(&dir)
            .write_manifest(&SessionManifest {
                schema: 1,
                id: name.into(),
                provider: "pi".into(),
                provider_path: "pi".into(),
                provider_version: "1.2.0".into(),
                workspace: root.to_path_buf(),
                title: name.into(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            })
            .unwrap();
        let mut records = Vec::new();
        // The first claim is the live one; a replacement claim is created by replacing it.
        for round in 0..2 {
            if round == 1 {
                if name != "claim_replaced_resets" {
                    break;
                }
                fs::remove_file(dir.join(CoreRecord::TurnClaim.name())).unwrap();
            }
            let pending = claim_pending_turn(&dir);
            let mut record = serde_json::json!({
                "token": pending.claim_token,
                "claim": fs::read_to_string(dir.join(CoreRecord::TurnClaim.name())).unwrap(),
                "prompt": correlated_prompt("prompt", &pending),
                "response": marked("result", &pending),
            });
            if name == "cancelled_keeps_usage" {
                // Cancel admission needs the extension's schema-2 readiness for this claim.
                write_json_atomic(
                    &dir.join(STARTUP_READY_FILE),
                    &StartupReady {
                        input_ack_schema: None,
                        schema: 2,
                        claim_token: pending.claim_token.clone(),
                        session_id: name.into(),
                    },
                )
                .unwrap();
                cancel::request(&Store::open_unchecked(&dir), name).unwrap();
                record["cancel"] = serde_json::json!(
                    fs::read_to_string(dir.join(CoreRecord::Cancel.name())).unwrap()
                );
                fs::remove_file(dir.join(CoreRecord::Cancel.name())).unwrap();
            }
            records.push(record);
        }
        inputs.insert(name.to_owned(), serde_json::Value::Array(records));
    }
    write_json_atomic(&root.join("inputs.json"), &inputs).unwrap();
    run_extension_harness(root, false);
    let outputs: Vec<serde_json::Value> = read_json(&root.join("payloads.json")).unwrap();
    let live_second_request = (793, 58, 16640, 0, Some(23), 17491);
    // (case, error, cancelled, usage (tokens), usage_error)
    for (name, error, cancelled, usage, usage_error) in [
        (
            "one_run_two_messages",
            None,
            false,
            Some(("complete", 1, 2, live_second_request)),
            None,
        ),
        (
            "two_runs",
            None,
            false,
            Some(("complete", 2, 2, (16800, 10, 0, 0, Some(0), 16810))),
            None,
        ),
        (
            "retry_without_before_agent_start",
            None,
            false,
            Some(("uncertain", 2, 2, (8401, 5, 0, 0, Some(0), 8406))),
            None,
        ),
        (
            "reasoning_omitted_unless_everywhere",
            None,
            false,
            Some(("complete", 1, 2, (793, 58, 16640, 0, None, 17491))),
            None,
        ),
        (
            "missing_usage",
            None,
            false,
            None,
            Some("assistant message has no usage"),
        ),
        (
            "unsafe_sum",
            None,
            false,
            None,
            Some("usage.cacheRead sum exceeds the safe integer range"),
        ),
        (
            "error_run_is_uncertain",
            Some("Pi turn ended with error: Network connection failed"),
            false,
            Some(("uncertain", 1, 2, live_second_request)),
            None,
        ),
        (
            "cancelled_keeps_usage",
            Some("cancelled: Pi turn ended with aborted"),
            true,
            Some(("uncertain", 1, 2, live_second_request)),
            None,
        ),
        (
            "claim_replaced_resets",
            None,
            false,
            Some(("complete", 1, 1, (623, 5, 8320, 0, Some(0), 8948))),
            None,
        ),
    ] {
        let dir = root.join(name);
        let payloads = outputs.iter().find(|v| v["name"] == name).unwrap()["payloads"]
            .as_array()
            .unwrap();
        assert_eq!(payloads.len(), 1, "{name}");
        ADAPTER.handle_hook(&dir, &payloads[0]).unwrap();
        let paths = event_paths(&dir).unwrap();
        assert_eq!(paths.len(), 1, "{name}");
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.error.as_deref(), error, "{name}");
        assert_eq!(event.cancelled, cancelled, "{name}");
        assert_eq!(event.usage_error.as_deref(), usage_error, "{name}");
        match usage {
            None => assert_eq!(event.usage, None, "{name}"),
            Some((coverage, runs, messages, tokens)) => {
                let usage = event.usage.expect(name);
                assert_eq!(usage.source, "pi_extension_agent_end", "{name}");
                assert_eq!(usage.scope, "request", "{name}");
                assert_eq!(usage.coverage, coverage, "{name}");
                assert_eq!(
                    (usage.runs, usage.assistant_messages),
                    (runs, messages),
                    "{name}"
                );
                assert_eq!(
                    (
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        usage.reasoning_tokens,
                        usage.total_tokens,
                    ),
                    tokens,
                    "{name}"
                );
            }
        }
        // The usage is part of the published bytes, so the journal predicate covers it.
        assert!(!dir.join(CoreRecord::TurnClaim.name()).exists(), "{name}");
    }
    // The failure signal keeps the reported usage, and the monitor records it.
    for (name, total, usage_error) in [
        ("delivery_failure_persists_usage", Some(8405), None),
        (
            "delivery_failure_persists_usage_error",
            None,
            Some("assistant message has no usage"),
        ),
    ] {
        let dir = root.join(name);
        let signal: serde_json::Value = read_json(&dir.join(HOOK_FAILURE_FILE)).unwrap();
        assert_eq!(signal["usage"]["total_tokens"].as_u64(), total, "{name}");
        assert_eq!(signal["usage_error"].as_str(), usage_error, "{name}");
        assert!(super::monitor::consume_hook_failure_for_test(&dir).unwrap());
        let paths = event_paths(&dir).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert!(
            event
                .error
                .as_deref()
                .unwrap()
                .contains("Pi result delivery failed")
        );
        assert_eq!(event.usage.map(|u| u.total_tokens), total, "{name}");
        assert_eq!(event.usage_error.as_deref(), usage_error, "{name}");
    }
}

#[test]
fn hook_payload_usage_is_validated_without_changing_the_outcome() {
    use crate::native::session::schema::SessionManifest;
    for (usage_field, expected_usage, expected_error) in [
        (
            serde_json::json!({"usage": {"coverage": "complete", "runs": 1, "assistant_messages": 1,
                "input_tokens": 8400, "output_tokens": 5, "cache_read_tokens": 0,
                "cache_write_tokens": 0, "reasoning_tokens": 0, "total_tokens": 8405}}),
            Some(8405),
            None,
        ),
        (
            serde_json::json!({"usage": {"coverage": "complete", "runs": -1, "assistant_messages": 1,
                "input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0,
                "cache_write_tokens": 0, "total_tokens": 2}}),
            None,
            Some("malformed usage: runs".to_owned()),
        ),
        (
            serde_json::json!({"usage_error": "x".repeat(400)}),
            None,
            Some("x".repeat(200)),
        ),
        (serde_json::json!({}), None, None),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        fs::create_dir(dir.join("events")).unwrap();
        update_status(dir, SessionState::Working, None, None).unwrap();
        Store::open_unchecked(dir)
            .write_manifest(&SessionManifest {
                schema: 1,
                id: "usage".into(),
                provider: "pi".into(),
                provider_path: "pi".into(),
                provider_version: "1.2.0".into(),
                workspace: dir.to_path_buf(),
                title: "usage".into(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            })
            .unwrap();
        let pending = claim_pending_turn(dir);
        let mut payload = serde_json::json!({
            "last_assistant_message": marked("result", &pending),
            "session_id": "pi-session", "turn_id": "leaf",
            "agent_bridge_claim_token": pending.claim_token,
            "agent_bridge_prompt_correlated": true,
        });
        for (key, value) in usage_field.as_object().unwrap() {
            payload[key] = value.clone();
        }
        ADAPTER.handle_hook(dir, &payload).unwrap();
        let paths = event_paths(dir).unwrap();
        assert_eq!(paths.len(), 1);
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "result");
        assert_eq!(event.error, None);
        assert_eq!(event.usage.map(|u| u.total_tokens), expected_usage);
        assert_eq!(event.usage_error, expected_error);
        assert!(!dir.join(TURN_CLAIM_FILE).exists());
    }
}

#[test]
fn pending_install_requires_a_live_session_and_cancel_is_unconditional() {
    use crate::native::session::CoreRecord;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_unchecked(directory.path());
    let first = claim_pending_turn(directory.path());
    let claim_path = store.record(CoreRecord::TurnClaim).path().to_owned();
    let closed_path = store.record(CoreRecord::Closed).path().to_owned();
    // Both installation paths: the launch plan (plain) and the delivery (bound).
    type Install = fn(&Path, &str) -> Result<PendingPiTurn>;
    let installs: [(&str, Install); 2] = [
        ("launch plan", |directory, token| {
            install_pending_turn(directory, token)
        }),
        ("delivery", |directory, token| {
            install_pending_turn_bound(directory, token, None)
        }),
    ];
    for (label, install) in installs {
        for state in [
            SessionState::Launching,
            SessionState::Running,
            SessionState::AwaitingInitialInput,
            SessionState::Ready,
            SessionState::Claimed,
            SessionState::Working,
            SessionState::ResumePending,
        ] {
            super::test_support::stored_state(directory.path(), &state);
            install(directory.path(), &first.claim_token)
                .unwrap_or_else(|error| panic!("{label} {state:?}: {error:#}"));
        }
        for state in [
            SessionState::Exited,
            SessionState::Failed,
            SessionState::Closed,
        ] {
            super::test_support::stored_state(directory.path(), &state);
            assert_eq!(
                install(directory.path(), &first.claim_token)
                    .unwrap_err()
                    .to_string(),
                "Pi session ended; no pending turn installed",
                "{label} {state:?}"
            );
        }
        super::test_support::stored_state(directory.path(), &SessionState::Working);
        fs::write(
            &closed_path,
            b"{\"state\":\"closed\",\"updated_unix_ms\":1}",
        )
        .unwrap();
        assert_eq!(
            install(directory.path(), &first.claim_token)
                .unwrap_err()
                .to_string(),
            "Pi session ended; no pending turn installed",
            "{label} tombstone"
        );
        fs::remove_file(&closed_path).unwrap();
        fs::write(&claim_path, b"999-999-999").unwrap();
        assert_eq!(
            install(directory.path(), &first.claim_token)
                .unwrap_err()
                .to_string(),
            "Pi pending turn no longer holds the session claim",
            "{label} replaced claim"
        );
        fs::write(&claim_path, first.claim_token.as_bytes()).unwrap();
    }
    // A replaced claim leaves the previous owner's pending record alone.
    fs::write(&claim_path, b"999-999-999").unwrap();
    cancel_pending_turn(directory.path(), &first.claim_token).unwrap();
    assert_eq!(
        read_pending_turn(directory.path())
            .unwrap()
            .unwrap()
            .claim_token,
        first.claim_token
    );
    // Cancel is cleanup: it checks the token only and tolerates unreadable state records.
    fs::write(&claim_path, first.claim_token.as_bytes()).unwrap();
    fs::write(store.record(CoreRecord::Status).path(), b"unreadable").unwrap();
    fs::write(&closed_path, b"unreadable").unwrap();
    cancel_pending_turn(directory.path(), &first.claim_token).unwrap();
    assert!(read_pending_turn(directory.path()).unwrap().is_none());
}
