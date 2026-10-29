use super::super::monitor::{MonitorState, pending_turn_failure, pending_turn_log};
use super::super::test_support::{
    REAL_DEFERRED_RELOAD_STARTUP, REAL_MACOS_INITIAL_TURN, REAL_QUIET_STARTUP,
    REAL_SECOND_CLUSTER_RELOAD_STARTUP, REAL_STARTUP_RELOAD_AFTER_STARTUP,
    REAL_SUCCESS_AFTER_RECEIPT, claim_pending_turn, framed_receipt_line, glog, go_quoted,
    late_reload_startup_log, marked, planner_line, receipt_line, settled_startup_log,
    successful_startup_log,
};
use super::super::{AGY_LOG_FILE, PendingAgyTurn, terminal_correlated_prompt};
use super::*;
use crate::native::session::primitives::{event_paths, read_json};
use crate::native::session::schema::SessionEvent;
use crate::native::session::{Reader, SessionState, Store, turn};
use crate::native::session::{SessionStatus, update_status};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

#[test]
fn diagnostics_describe_agy_owned_fallback_without_claiming_live_delivery() {
    use super::super::super::super::diagnostics::{Availability, Context};
    use super::super::NativeProviderAdapter;
    let checks = super::super::ADAPTER.diagnose(Context {
        directory: None,
        manifest: None,
        executable: None,
        current_version: None,
        workspace: std::path::Path::new("."),
        probe: false,
        deadline: std::time::Instant::now(),
        environment: &crate::native::consent::Environment::default(),
    });
    assert_eq!(checks[0].reason_code, "agy_terminal_fallback");
    assert_eq!(checks[0].availability, Availability::Unknown);
}

#[test]
fn doctor_reports_startup_readiness_and_the_pending_receipt_without_recovery() {
    use super::super::super::super::diagnostics::Availability;
    let check = input_receipt_check(None);
    assert_eq!(check.id, "agy_input_receipt");
    assert_eq!(check.availability, Availability::Unknown);
    assert_eq!(check.reason_code, "agy_log_unavailable");

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-doctor1");
    fs::create_dir_all(directory.join("events")).unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    assert_eq!(
        input_receipt_check(Some(&directory)).reason_code,
        "agy_log_missing"
    );

    let log_path = directory.join(AGY_LOG_FILE);
    let startup_only = glog(
        "16:41:07.830345",
        1,
        "analytics.go:187",
        "CLI startup completed (took 1ms)",
    );
    fs::write(&log_path, &startup_only).unwrap();
    // The Windows console rule requires the redraw after startup.
    let check = input_receipt_check_for_platform(Some(&directory), true);
    assert_eq!(check.reason_code, "agy_startup_markers_missing");
    let check = serde_json::to_value(&check).unwrap();
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("Full redraw completed")
    );
    let evidence = check["evidence"].clone();
    assert_eq!(evidence["startup_completed"], true);
    assert_eq!(evidence["redraw_required"], true);
    assert_eq!(evidence["redraw_after_startup_observed"], false);
    assert_eq!(evidence["deferred_reload_after_startup_observed"], false);
    assert_eq!(evidence["startup_reload_after_startup_ignored"], false);
    assert_eq!(evidence["conversation_reload_after_startup_ignored"], false);
    assert_eq!(evidence["activity_lines"], 0);
    assert!(evidence.get("latest_reload_completed").is_none());
    // The macOS rule does not: startup alone completes the markers there.
    let check = input_receipt_check_for_platform(Some(&directory), false);
    assert_eq!(check.reason_code, "agy_no_input_receipt");
    let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
    assert_eq!(evidence["redraw_required"], false);
    assert_eq!(evidence["redraw_after_startup_observed"], false);
    // The real macOS log: the conversation reload is reported as ignored, the
    // trust reload as observed (the evidence keeps its "deferred" field name),
    // and the smoke's lost paste left no receipt.
    fs::write(&log_path, REAL_MACOS_INITIAL_TURN).unwrap();
    let check = input_receipt_check_for_platform(Some(&directory), false);
    assert_eq!(check.reason_code, "agy_no_input_receipt");
    let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
    assert_eq!(evidence["conversation_reload_after_startup_ignored"], true);
    assert_eq!(evidence["deferred_reload_after_startup_observed"], true);
    assert_eq!(
        input_receipt_check_for_platform(Some(&directory), true).reason_code,
        "agy_startup_markers_missing",
        "the same log never satisfies the Windows rule"
    );

    for log in [
        REAL_QUIET_STARTUP.to_owned(),
        REAL_DEFERRED_RELOAD_STARTUP.to_owned(),
        late_reload_startup_log(),
        settled_startup_log(),
        REAL_SECOND_CLUSTER_RELOAD_STARTUP.to_owned(),
        REAL_STARTUP_RELOAD_AFTER_STARTUP.to_owned(),
        successful_startup_log(),
    ] {
        fs::write(&log_path, log).unwrap();
        assert_eq!(
            input_receipt_check_for_platform(Some(&directory), true).reason_code,
            "agy_no_input_receipt"
        );
    }
    // session-udT6uY: the skills reload 0.5 ms after startup is reported as the
    // ignored startup reload, not as the deferred one.
    let evidence = serde_json::to_value(input_receipt_check_for_platform(Some(&directory), true))
        .unwrap()["evidence"]
        .clone();
    assert_eq!(evidence["deferred_reload_after_startup_observed"], false);
    assert_eq!(evidence["startup_reload_after_startup_ignored"], true);

    fs::write(&log_path, settled_startup_log()).unwrap();
    let pending = claim_pending_turn(&directory);
    let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
    write!(log, "{}", framed_receipt_line("hello", &pending)).unwrap();
    drop(log);
    let check = input_receipt_check_for_platform(Some(&directory), true);
    assert_eq!(check.reason_code, "agy_input_receipt_observed");
    let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
    assert_eq!(evidence["startup_completed"], true);
    assert_eq!(evidence["redraw_after_startup_observed"], true);
    assert_eq!(evidence["deferred_reload_after_startup_observed"], true);
    assert_eq!(evidence["startup_reload_after_startup_ignored"], false);
    assert_eq!(evidence["input_receipts"], 1);
    assert_eq!(evidence["last_receipt_truncated"], false);
    assert_eq!(evidence["pending_marker_received"], true);
    assert_eq!(evidence["pending_marker"], pending.marker);
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "working");
}

// Composite fixture: existing real startup/receipt excerpts (2026-09-24),
// the Forwarding line from the real 2026-10-01 quota fixture, and the exact
// confirmation text observed once with Agy 1.3.0 on 2026-10-07 in issue #82.
// This is not a captured continuous 1.3.0 log; no command text was observed.
const APPROVAL_FORWARDED: &str = "I1001 17:23:48.670594     402 conversation_manager.go:699] Forwarding user message to conversation 97ad12fd-9e7a-4556-83a4-8f0147343657 (items=1, media=0)\n";
const APPROVAL_LINE: &str = "Surfacing tool confirmation: \"RunCommand\" at step 2\n";

fn approval_log(pending: &PendingAgyTurn) -> String {
    REAL_QUIET_STARTUP.to_owned()
        + &receipt_line(&go_quoted(
            &terminal_correlated_prompt("reply", pending, false).unwrap(),
        ))
        + REAL_SUCCESS_AFTER_RECEIPT
        + "Created conversation 97ad12fd-9e7a-4556-83a4-8f0147343657\n"
        + APPROVAL_FORWARDED
        + APPROVAL_LINE
}

#[test]
fn tool_confirmation_uses_failure_binding_and_rejects_other_turns() {
    let pending = PendingAgyTurn::new("82-1-0").unwrap();
    let log = approval_log(&pending);
    assert_eq!(
        pending_turn_log(log.as_bytes(), &pending)
            .confirmation
            .as_deref(),
        Some("RunCommand")
    );
    for log in [
        APPROVAL_LINE.to_owned() + APPROVAL_FORWARDED,
        log.clone() + APPROVAL_FORWARDED + APPROVAL_LINE,
        approval_log(&PendingAgyTurn::new("82-2-0").unwrap()),
    ] {
        assert!(
            pending_turn_log(log.as_bytes(), &pending)
                .confirmation
                .is_none()
        );
    }
    let failed = log + "agent executor error: quota exhausted\n";
    let observed = pending_turn_log(failed.as_bytes(), &pending);
    assert_eq!(observed.failure.as_deref(), Some("quota exhausted"));
    assert_eq!(
        pending_turn_failure(failed.as_bytes(), &pending),
        Some(("quota exhausted".to_owned(), true))
    );
}

#[test]
fn tool_confirmation_is_read_only_and_yields_to_results_and_failures() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session-approval");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let store = Store::open_unchecked(&directory);
    store
        .write_manifest(&crate::native::session::schema::SessionManifest {
            schema: 1,
            id: "session-approval".to_owned(),
            provider: "agy".to_owned(),
            provider_path: PathBuf::from("agy"),
            provider_version: "fixture 1.3.0".to_owned(),
            workspace: directory.clone(),
            title: "approval fixture".to_owned(),
            model: None,
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        })
        .unwrap();
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let pending = claim_pending_turn(&directory);
    let brain = root.path().join("brain");
    let logs = brain.join("97ad12fd-9e7a-4556-83a4-8f0147343657/.system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    let transcript = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    fs::write(&transcript, "").unwrap();
    let log_path = directory.join(AGY_LOG_FILE);
    let log = approval_log(&pending);
    crate::native::session::primitives::write_private(&log_path, log.as_bytes()).unwrap();
    let before = fs::read(directory.join("status.json")).unwrap();
    let request = crate::native::session::requests::for_claim(
        &Reader::open_unchecked(&directory),
        &pending.claim_token,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        pending_tool_confirmation(&directory, &brain).unwrap(),
        Some(("RunCommand".to_owned(), request.request_id))
    );
    assert_eq!(fs::read(directory.join("status.json")).unwrap(), before);
    assert!(event_paths(&directory).unwrap().is_empty());
    assert_eq!(
        turn::current_claim_token(&Reader::open_unchecked(&directory)).unwrap(),
        Some(pending.claim_token.clone())
    );

    // No receipt: the argument-delivered initial turn needs the sole full input.
    let initial = "Created conversation 97ad12fd-9e7a-4556-83a4-8f0147343657\n".to_owned()
        + APPROVAL_FORWARDED
        + APPROVAL_LINE;
    fs::write(&log_path, &initial).unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    let input =
        serde_json::json!({"type":"USER_INPUT", "content": pending.marker}).to_string() + "\n";
    fs::write(&full, &input).unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_some()
    );
    fs::write(&full, input.clone() + &input).unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    fs::write(&log_path, &log).unwrap();
    update_status(
        &directory,
        SessionState::Working,
        None,
        Some("receipt missing".to_owned()),
    )
    .unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    update_status(&directory, SessionState::Working, None, None).unwrap();
    fs::write(
        &log_path,
        log.clone() + "agent executor error: quota exhausted\n",
    )
    .unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    fs::write(&log_path, &log).unwrap();
    fs::write(
        &transcript,
        planner_line(3, &marked("done", &pending)) + "\n",
    )
    .unwrap();
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    // Diagnosis did not consume the result: the ordinary monitor still records it.
    MonitorState::default()
        .poll(&directory, &log_path, &brain)
        .unwrap();
    assert_eq!(event_paths(&directory).unwrap().len(), 1);
    assert!(
        pending_tool_confirmation(&directory, &brain)
            .unwrap()
            .is_none()
    );
    let event: SessionEvent = read_json(&event_paths(&directory).unwrap()[0]).unwrap();
    assert_eq!(event.message, "done");
    assert!(event.error.is_none());
}
