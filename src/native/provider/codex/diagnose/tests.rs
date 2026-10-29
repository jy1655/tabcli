#[cfg(unix)]
use super::super::test_support::write_established_thread;
use super::*;

fn lifecycle_evidence(directory: &Path) -> serde_json::Value {
    serde_json::to_value(lifecycle::check(Some(directory))).unwrap()["evidence"].clone()
}

#[test]
fn lifecycle_doctor_legacy_records_are_not_observed_and_write_nothing() {
    let root = tempfile::tempdir().unwrap();
    let before = std::fs::read_dir(root.path()).unwrap().count();
    let evidence = lifecycle_evidence(root.path());
    for field in [
        "launch_mode",
        "input_hook_observed",
        "stop_candidate_observed",
        "observer",
        "terminal_outcome",
    ] {
        assert_eq!(evidence[field]["observation"], "not_observed", "{field}");
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
}

#[test]
fn lifecycle_doctor_records_current_and_last_claim_without_mutation() {
    use crate::native::session::{SessionState, Store, turn, update_status};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let dir = super::super::test_support::launch_session(root.path(), Some("0.164.0"));
    let store = Store::open_unchecked(&dir);
    update_status(&dir, SessionState::Working, None, None).unwrap();
    let token = turn::current_claim_token(&Reader::open_unchecked(&dir))
        .unwrap()
        .unwrap();
    let pending = super::super::install_pending_turn_for_prompt(&dir, &token, "hello").unwrap();
    let exe = Path::new(if cfg!(windows) {
        r"C:\tabcli.exe"
    } else {
        "/opt/tabcli"
    });
    super::super::hooks::prepare(&dir, exe).unwrap();
    let binding = super::super::turn::Binding {
        schema: 1,
        claim_token: token.clone(),
        thread_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
        turn_id: "turn-1".into(),
        transcript_path: "rollout.jsonl".into(),
        bound_unix_ms: 42,
    };
    store
        .private(&format!("codex-turn-{token}.json"))
        .write_json(&binding)
        .unwrap();
    store.private(&format!("codex-candidate-{token}.json")).write_json(&json!({
        "schema":1,"claim_token":token,"thread_id":binding.thread_id,"turn_id":"turn-1",
        "stop_hook_active":false,"last_assistant_message":"not the result", "observed_unix_ms":43
    })).unwrap();
    for state in ["armed", "suspended", "settled"] {
        store.private(&format!("codex-observer-{token}.json")).write_json(&json!({
            "schema":1,"claim_token":token,"state":state,"reason":"observation_unavailable: truncated", "observed_unix_ms":44
        })).unwrap();
        let evidence = lifecycle_evidence(&dir);
        assert_eq!(
            evidence["launch_mode"]["record"]["lifecycle_hooks"],
            "injected"
        );
        assert!(
            evidence["launch_mode"]["warning"]
                .as_str()
                .unwrap()
                .contains("differs")
        );
        assert_eq!(
            evidence["input_hook_observed"]["record"]["bound_unix_ms"],
            42
        );
        assert_eq!(
            evidence["stop_candidate_observed"]["record"]["observed_unix_ms"],
            43
        );
        assert!(
            evidence["stop_candidate_observed"]["record"]
                .get("last_assistant_message")
                .is_none()
        );
        assert_eq!(evidence["observer"]["record"]["state"], state);
        assert_eq!(evidence["terminal_outcome"]["observation"], "not_observed");
    }
    assert_eq!(
        notify_payload_check(Some(&dir)).reason_code,
        "codex_notify_payload_not_required"
    );
    assert!(pending.framed_prompt_chars.is_some());
    turn::Report::for_claim(&store, agent_bridge::FirstPartyCli::Codex, Some(&token))
        .with_provenance(Some("codex_transcript_turn_aborted"))
        .fail(
            "interrupted",
            Some(binding.thread_id),
            Some("turn-1".into()),
        )
        .unwrap();
    fn snapshot(path: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rows.extend(snapshot(&path));
            } else {
                rows.push((path.clone(), std::fs::read(path).unwrap()));
            }
        }
        rows.sort();
        rows
    }
    let before = snapshot(&dir);
    let evidence = lifecycle_evidence(&dir);
    assert_eq!(evidence["claim_token"], token);
    assert_eq!(
        evidence["terminal_outcome"]["record"]["provenance"],
        "codex_transcript_turn_aborted"
    );
    assert_eq!(
        evidence["terminal_outcome"]["record"]["kind"],
        "turn_aborted"
    );
    assert_eq!(
        evidence["terminal_outcome"]["record"]["request_state"],
        "failed"
    );
    assert_eq!(snapshot(&dir), before);
    // A newer pending request must not inherit the previous committed outcome.
    turn::claim(&store, &[]).unwrap().retain();
    let evidence = lifecycle_evidence(&dir);
    assert_ne!(evidence["claim_token"], token);
    assert_eq!(evidence["terminal_outcome"]["observation"], "not_observed");
    assert_eq!(
        evidence["input_hook_observed"]["observation"],
        "not_observed"
    );
}

#[test]
fn lifecycle_doctor_terminal_kinds_come_only_from_committed_provenance() {
    use crate::native::session::{SessionState, Store, turn, update_status};
    for (source, kind) in [
        (None, None),
        (Some("codex_notify"), Some("agent-turn-complete")),
        (
            Some("codex_transcript_task_complete"),
            Some("task_complete"),
        ),
        (Some("codex_transcript_turn_aborted"), Some("turn_aborted")),
    ] {
        let root = tempfile::tempdir().unwrap();
        let dir = super::super::test_support::launch_session(root.path(), Some("0.163.0"));
        let store = Store::open_unchecked(&dir);
        update_status(&dir, SessionState::Working, None, None).unwrap();
        let token = turn::current_claim_token(&Reader::open_unchecked(&dir))
            .unwrap()
            .unwrap();
        let report =
            turn::Report::for_claim(&store, agent_bridge::FirstPartyCli::Codex, Some(&token))
                .with_provenance(source);
        if kind == Some("turn_aborted") {
            report
                .fail("Codex did not accept the delegated prompt", None, None)
                .unwrap();
        } else {
            report.complete("answer", None, None).unwrap();
        }
        let evidence = lifecycle_evidence(&dir);
        if let Some(kind) = kind {
            assert_eq!(evidence["terminal_outcome"]["record"]["kind"], kind);
            if kind == "turn_aborted" {
                assert_eq!(
                    evidence["terminal_outcome"]["record"]["request_state"],
                    "failed"
                );
            }
            assert_eq!(
                evidence["terminal_outcome"]["record"]["provenance"],
                source.unwrap()
            );
        } else {
            for field in [
                "launch_mode",
                "input_hook_observed",
                "stop_candidate_observed",
                "observer",
                "terminal_outcome",
            ] {
                assert_eq!(evidence[field]["observation"], "not_observed");
            }
        }
    }
}

#[test]
fn notify_budget_remains_for_injected_unbound_and_not_injected_claims() {
    use crate::native::session::{Store, turn};
    let root = tempfile::tempdir().unwrap();
    let dir = super::super::test_support::launch_session(root.path(), Some("0.163.0"));
    let token = turn::current_claim_token(&Reader::open_unchecked(&dir))
        .unwrap()
        .unwrap();
    super::super::install_pending_turn_for_prompt(&dir, &token, "hello").unwrap();
    let exe = Path::new(if cfg!(windows) {
        r"C:\tabcli.exe"
    } else {
        "/opt/tabcli"
    });
    super::super::hooks::prepare(&dir, exe).unwrap();
    for injected in [true, false] {
        if !injected {
            Store::open_unchecked(&dir)
                .private("codex-launch-mode.json")
                .write_json(&serde_json::json!({
                    "schema":1,"lifecycle_hooks":"not_injected","reason":"settings_off",
                    "codex_version":"0.163.0","vector_codex_version":"0.163.0"
                }))
                .unwrap();
        }
        let row = serde_json::to_value(notify_payload_check(Some(&dir))).unwrap();
        assert_eq!(row["evidence"]["source"], "pending_turn_record");
        assert_eq!(row["evidence"]["limit_units"], 32767);
    }
}
#[cfg(unix)]
use std::time::Instant;

#[test]
fn diagnostics_keep_launch_version_and_unprobed_daemon_observations_explicit() {
    use crate::native::diagnostics::{Availability, Context};
    for (version, expected) in [
        (Some("0.148.0"), Availability::Unavailable),
        (Some("0.153.2"), Availability::Available),
        (Some("unknown"), Availability::Unknown),
        (None, Availability::Unknown),
    ] {
        let checks = super::diagnose_codex(Context {
            directory: None,
            manifest: None,
            executable: None,
            current_version: version,
            workspace: std::path::Path::new("."),
            probe: false,
            deadline: std::time::Instant::now(),
            environment: &crate::native::consent::Environment::default(),
        });
        assert!(!checks.iter().any(|c| c.id == "codex_hook_trust"));
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_queue_version")
                .unwrap()
                .availability,
            expected
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_daemon")
                .unwrap()
                .availability,
            Availability::Unknown
        );
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_thread")
                .unwrap()
                .reason_code,
            "session_required"
        );
        let mcp = checks.iter().find(|c| c.id == "codex_mcp").unwrap();
        assert_eq!(mcp.availability, Availability::Unknown);
        assert_eq!(mcp.reason_code, "codex_mcp_not_observed");
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "codex_terminal_follow_up")
                .unwrap()
                .availability,
            Availability::Unavailable
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn diagnostic_daemon_availability_describes_the_optional_shared_server() {
    use crate::native::diagnostics::Availability::{Available, Unavailable, Unknown};
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    for (success, payload, expected) in [
        (
            true,
            r#"{"status":"running","appServerVersion":"0.153.2"}"#,
            Available,
        ),
        (true, r#"{"status":"stopped"}"#, Unavailable),
        (
            true,
            r#"{"status":"running","appServerVersion":"0.148.0"}"#,
            Unavailable,
        ),
        (true, r#"{"status":"running"}"#, Unknown),
        (true, "invalid json", Unknown),
        (false, "", Unavailable),
    ] {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: payload.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        let check = super::diagnose_daemon_output(&output, "0.153.2");
        assert_eq!(check.availability, expected, "{payload}");
        let reported = serde_json::to_value(&check).unwrap();
        assert!(
            reported["next_action"]
                .as_str()
                .unwrap()
                .contains("a shared daemon is not required")
        );
        assert_eq!(
            check.availability == Available,
            super::classify_codex_daemon_probe(
                success,
                &output.stdout,
                &output.stderr,
                false,
                "0.153.2"
            )
            .is_ok()
        );
    }
}

#[cfg(unix)]
#[test]
fn successful_model_result_does_not_mark_mcp_connected() {
    use crate::native::diagnostics::{Availability, Context};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexqueue");
    std::fs::create_dir(&directory).unwrap();
    write_established_thread(&directory, "018f0000-0000-7000-8000-000000000001");
    let checks = diagnose_codex(Context {
        directory: Some(&directory),
        manifest: None,
        executable: None,
        current_version: Some("0.159.3"),
        workspace: root.path(),
        probe: false,
        deadline: Instant::now(),
        environment: &crate::native::consent::Environment::default(),
    });
    assert_eq!(
        checks
            .iter()
            .find(|c| c.id == "codex_thread")
            .unwrap()
            .availability,
        Availability::Available
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c.id == "codex_mcp")
            .unwrap()
            .availability,
        Availability::Unknown
    );
}

#[test]
fn codex_daemon_probe_requires_a_running_compatible_app_server() {
    assert!(
        classify_codex_daemon_probe(
            true,
            br#"{"status":"running","appServerVersion":"0.149.0"}"#,
            b"",
            false,
            "codex-cli 0.153.2",
        )
        .is_ok()
    );

    for (success, stdout, stderr, truncated) in [
        (false, b"".as_slice(), b"socket missing".as_slice(), false),
        (
            true,
            br#"{"status":"running","appServerVersion":"0.153.2"}"#.as_slice(),
            b"".as_slice(),
            true,
        ),
        (true, b"not json".as_slice(), b"".as_slice(), false),
        (
            true,
            br#"{"status":"stopped","appServerVersion":"0.153.2"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
        (
            true,
            br#"{"status":"running"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
        (
            true,
            br#"{"status":"running","appServerVersion":"0.148.9"}"#.as_slice(),
            b"".as_slice(),
            false,
        ),
    ] {
        assert!(
            classify_codex_daemon_probe(success, stdout, stderr, truncated, "codex-cli 0.153.2",)
                .is_err()
        );
    }
}

#[test]
fn notify_payload_budget_counts_json_escaped_utf16_units() {
    use super::{NOTIFY_COMMAND_LINE_LIMIT_UNITS, notify_payload_budget};
    let ascii = notify_payload_budget("abcd", 100);
    assert_eq!(ascii.prompt_chars, 4);
    assert_eq!(ascii.escaped_units, 6, "two quotes around four characters");
    assert_eq!(ascii.envelope_units, 400);
    assert_eq!(ascii.limit_units, NOTIFY_COMMAND_LINE_LIMIT_UNITS);
    assert_eq!(
        ascii.answer_budget_units,
        NOTIFY_COMMAND_LINE_LIMIT_UNITS - 406
    );

    let escaped = notify_payload_budget("a\"b\\c\nd", 0);
    assert_eq!(escaped.prompt_chars, 7);
    assert_eq!(
        escaped.escaped_units, 12,
        "quote, backslash and newline each take two units"
    );

    let non_bmp = notify_payload_budget("😀", 0);
    assert_eq!(non_bmp.prompt_chars, 1);
    assert_eq!(non_bmp.escaped_units, 4, "two quotes plus a surrogate pair");
}

#[test]
fn notify_payload_budget_classifies_by_host_and_remaining_budget() {
    use super::{
        NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS, NOTIFY_COMMAND_LINE_LIMIT_UNITS,
        notify_payload_budget,
    };
    use crate::native::diagnostics::Availability::{Available, Unavailable};
    assert_eq!(NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS, 4_096);
    let short = notify_payload_budget("short", 100);
    let near = notify_payload_budget(
        &"x".repeat(
            NOTIFY_COMMAND_LINE_LIMIT_UNITS - 400 - 2 - NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS + 1,
        ),
        100,
    );
    assert_eq!(
        near.answer_budget_units,
        NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS - 1
    );
    let within = notify_payload_budget(
        &"x".repeat(
            NOTIFY_COMMAND_LINE_LIMIT_UNITS - 400 - 2 - NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS,
        ),
        100,
    );
    assert_eq!(
        within.answer_budget_units,
        NOTIFY_ANSWER_BUDGET_THRESHOLD_UNITS
    );
    let over = notify_payload_budget(&"x".repeat(NOTIFY_COMMAND_LINE_LIMIT_UNITS - 400 - 2), 100);
    assert_eq!(over.answer_budget_units, 0);
    let beyond = notify_payload_budget(&"x".repeat(NOTIFY_COMMAND_LINE_LIMIT_UNITS), 100);
    assert_eq!(beyond.answer_budget_units, 0);
    for (budget, expected) in [
        (&short, (Available, "codex_notify_payload_within_limit")),
        (&within, (Available, "codex_notify_payload_within_limit")),
        (&near, (Unavailable, "codex_notify_payload_near_limit")),
        (&over, (Unavailable, "codex_notify_payload_over_limit")),
        (&beyond, (Unavailable, "codex_notify_payload_over_limit")),
    ] {
        assert_eq!(classify_notify_payload_budget(budget, true), expected);
        assert_eq!(
            classify_notify_payload_budget(budget, false),
            (Available, "codex_notify_payload_no_limit")
        );
    }
}

#[test]
fn notify_payload_check_reads_the_pending_initial_prompt_and_reports_unknown_otherwise() {
    use crate::native::diagnostics::{Availability, Context};
    use crate::native::session::{CoreRecord, Store};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-codexbudget");
    std::fs::create_dir(&directory).unwrap();
    let environment = crate::native::consent::Environment::default();
    let check = |directory: Option<&std::path::Path>| {
        let checks = diagnose_codex(Context {
            directory,
            manifest: None,
            executable: None,
            current_version: Some("0.159.3"),
            workspace: root.path(),
            probe: false,
            deadline: std::time::Instant::now(),
            environment: &environment,
        });
        let check = checks
            .into_iter()
            .find(|c| c.id == "codex_notify_payload_budget")
            .unwrap();
        let row = serde_json::to_value(&check).unwrap();
        (check.availability, check.reason_code, row)
    };
    let (availability, reason, _) = check(None);
    assert_eq!(
        (availability, reason),
        (Availability::Unknown, "session_required")
    );

    let (availability, reason, _) = check(Some(&directory));
    assert_eq!(
        (availability, reason),
        (Availability::Unknown, "codex_turn_not_pending")
    );

    let claim = "1700000000000-1";
    super::super::install_pending_turn(&directory, claim).unwrap();
    let (availability, reason, row) = check(Some(&directory));
    assert_eq!(
        (availability, reason),
        (Availability::Unknown, "codex_pending_prompt_unavailable")
    );
    assert!(
        row["evidence"]["error"]
            .as_str()
            .unwrap()
            .contains("carries no prompt sizes")
    );

    Store::open_unchecked(&directory)
        .record(CoreRecord::InitialPrompt)
        .write_private(b"hello")
        .unwrap();
    let (availability, reason, row) = check(Some(&directory));
    if cfg!(windows) {
        assert_eq!(
            (availability, reason),
            (Availability::Available, "codex_notify_payload_within_limit")
        );
    } else {
        assert_eq!(
            (availability, reason),
            (Availability::Available, "codex_notify_payload_no_limit")
        );
    }
    let evidence = &row["evidence"];
    assert_eq!(evidence["source"], "initial_prompt_record");
    assert_eq!(evidence["limit_units"], 32_767);
    assert_eq!(evidence["threshold_units"], 4_096);
    assert!(evidence["prompt_chars"].as_u64().unwrap() > "hello".len() as u64);
    assert!(
        evidence["escaped_units"].as_u64().unwrap() > evidence["prompt_chars"].as_u64().unwrap()
    );
    assert!(evidence["envelope_units"].as_u64().unwrap() >= 300);
    assert!(evidence["answer_budget_units"].as_u64().unwrap() > 4_096);
    assert!(evidence["pending_request"].is_null());
    assert!(row["next_action"].as_str().unwrap().contains("rollout"));
    assert!(
        row["detail"]
            .as_str()
            .unwrap()
            .contains("not a measurement")
    );
    assert_eq!(evidence["executable_source"], "doctor_executable");

    // A pending turn record that carries the sizes wins over the preserved initial prompt
    // while both exist (`hello` would give a far smaller prompt), and still answers once
    // delivery has removed the initial prompt record.
    let delivered = "x".repeat(10_000);
    let pending =
        super::super::install_pending_turn_for_prompt(&directory, "1700000000000-3", &delivered)
            .unwrap();
    let (_, _, row) = check(Some(&directory));
    assert_eq!(row["evidence"]["source"], "pending_turn_record");
    assert_eq!(
        row["evidence"]["prompt_chars"],
        pending.framed_prompt_chars.unwrap()
    );
    std::fs::remove_file(
        Store::open_unchecked(&directory)
            .record(CoreRecord::InitialPrompt)
            .path(),
    )
    .unwrap();
    let (availability, reason, row) = check(Some(&directory));
    assert_eq!(
        reason,
        if cfg!(windows) {
            "codex_notify_payload_within_limit"
        } else {
            "codex_notify_payload_no_limit"
        }
    );
    assert_eq!(availability, Availability::Available);
    let evidence = &row["evidence"];
    assert_eq!(evidence["source"], "pending_turn_record");
    assert_eq!(
        evidence["prompt_chars"],
        pending.framed_prompt_chars.unwrap()
    );
    assert_eq!(
        evidence["escaped_units"],
        pending.framed_prompt_units.unwrap()
    );
    assert!(evidence["prompt_chars"].as_u64().unwrap() > 10_000);
    assert!(evidence["answer_budget_units"].as_u64().unwrap() < 32_767 - 10_000);
}

fn classify_notify_payload_budget(
    budget: &super::NotifyPayloadBudget,
    windows: bool,
) -> (crate::native::diagnostics::Availability, &'static str) {
    let (availability, reason) = super::classify_notify_payload_budget(budget, windows);
    (availability, reason.name())
}
