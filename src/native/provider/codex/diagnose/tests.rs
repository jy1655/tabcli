#[cfg(unix)]
use super::super::test_support::write_established_thread;
use super::*;
#[cfg(unix)]
use std::time::Instant;

#[test]
fn diagnostics_keep_launch_version_and_unprobed_daemon_observations_explicit() {
    use crate::native::doctor::{Availability, Context};
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
    use crate::native::doctor::Availability::*;
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
    use crate::native::doctor::{Availability, Context};
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
        classify_notify_payload_budget, notify_payload_budget,
    };
    use crate::native::doctor::Availability::*;
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
    use crate::native::doctor::{Availability, Context};
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
    assert!(row["detail"].as_str().unwrap().contains("lower bound"));

    // A delivered turn: the initial prompt record is gone, and the pending turn record
    // carries the sizes the install wrote. The record wins even while the prompt exists.
    let delivered = "x".repeat(10_000);
    let pending =
        super::super::install_pending_turn_for_prompt(&directory, "1700000000000-3", &delivered)
            .unwrap();
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
        pending.delivered_prompt_chars.unwrap()
    );
    assert_eq!(
        evidence["escaped_units"],
        pending.delivered_prompt_units.unwrap()
    );
    assert!(evidence["prompt_chars"].as_u64().unwrap() > 10_000);
    assert!(evidence["answer_budget_units"].as_u64().unwrap() < 32_767 - 10_000);
}
