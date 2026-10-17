//! Origin observation through the public commands: the same fields in every query, the
//! status attention and the doctor check, and the bounded report after a close.
mod support;
use support::{entries, manifest, status, success, write};

use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Root {
    root: tempfile::TempDir,
    workspace: PathBuf,
}

impl Root {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        Self { root, workspace }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn session(&self, id: &str, state: &str, source: Option<&str>) -> PathBuf {
        let directory = self.path().join(id);
        fs::create_dir_all(directory.join("events")).unwrap();
        write(
            &directory.join("manifest.json"),
            &manifest(
                id,
                "claude",
                "claude".as_ref(),
                "2.1.280",
                &self.workspace,
                id,
            ),
        );
        write(&directory.join("status.json"), &status(state, 2, None));
        if let Some(source) = source {
            write(
                &directory.join("launch.json"),
                &json!({"schema": 1, "claim_token": "100-1", "deadline_unix_ms": 1,
                    "phase": "spawned"}),
            );
            fs::create_dir(directory.join("requests")).unwrap();
            write(
                &directory.join("requests/100-1.json"),
                &json!({"schema": 1, "request_id": format!("request-{id}"),
                    "claim_token": "100-1", "event_file": "event-100-1.json",
                    "created_unix_ms": 3, "source": source}),
            );
        }
        directory
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.path())
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .output()
            .unwrap()
    }

    fn status_of(&self, id: &str) -> Value {
        let value = success(self.run(&["status", "--all-workspaces", "--all", "--json"]));
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    }

    fn sessions_of(&self, id: &str) -> Value {
        let output = self.run(&["sessions", "--json"]);
        assert!(output.status.success());
        let sessions: Value = serde_json::from_slice(&output.stdout).unwrap();
        sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    }

    fn doctor_check(&self, id: &str) -> (Value, Value) {
        let output = self.run(&["doctor", id, "--json"]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "origin")
            .unwrap()
            .clone();
        (report, check)
    }
}

fn fields(value: &Value) -> Value {
    json!({"origin": value["origin"], "origin_observation": value["origin_observation"]})
}

#[test]
fn every_query_renders_the_same_origin_and_only_status_flags_an_ended_one() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    root.session("session-child", "ready", Some("session-origin"));
    root.session("session-external", "ready", Some("external"));
    root.session("session-legacy", "ready", None);
    let before = entries(root.path());

    let child = root.status_of("session-child");
    let expected = json!({"origin": "session-origin", "origin_observation": {
        "kind": "managed", "request_id": "request-session-child", "stored_state": "failed",
        "ended": true, "reason": "recorded_failed", "detail": null, "derived_from": "records"}});
    assert_eq!(fields(&child), expected);
    assert_eq!(
        child["attention"].as_array().unwrap().last().unwrap(),
        "origin_ended"
    );
    let inspect = success(root.run(&["inspect", "session-child", "--json"]));
    assert_eq!(fields(&inspect), expected);
    let (doctor, check) = root.doctor_check("session-child");
    assert_eq!(fields(&doctor["observations"]), expected);
    assert_eq!(check["evidence"], expected);
    assert_eq!(check["availability"], "unavailable");
    assert_eq!(check["reason_code"], "origin_ended");
    assert_eq!(
        check["next_action"],
        "If this session is no longer needed, close it explicitly; the origin observation grants no close authority."
    );
    assert_eq!(
        check["next_command"],
        json!(["tabcli", "close-session", "session-child", "--explicit"])
    );
    // Queries observe only.
    assert_eq!(entries(root.path()), before);
    assert_eq!(fields(&root.sessions_of("session-child")), expected);

    let external = root.status_of("session-external");
    assert_eq!(external["origin"], Value::Null);
    assert_eq!(external["origin_observation"]["kind"], "external");
    assert!(
        !external["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("origin_ended"))
    );
    let (_, check) = root.doctor_check("session-external");
    assert_eq!(
        (
            check["availability"].as_str(),
            check["reason_code"].as_str()
        ),
        (Some("available"), Some("origin_external"))
    );
    let legacy = root.status_of("session-legacy");
    assert_eq!(legacy["origin"], Value::Null);
    assert_eq!(legacy["origin_observation"]["kind"], "unknown");
    let (_, check) = root.doctor_check("session-legacy");
    assert_eq!(
        (
            check["availability"].as_str(),
            check["reason_code"].as_str()
        ),
        (Some("unknown"), Some("origin_unverified"))
    );
    assert!(check["next_command"].is_null());
    // The ended origin itself has no recorded origin, and a failed state is not closed.
    assert!(
        !root.status_of("session-origin")["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("origin_ended"))
    );
}

#[test]
fn a_live_origin_is_live_and_a_closed_session_needs_no_action() {
    let root = Root::new();
    let origin = root.session("session-origin", "ready", None);
    write(
        &origin.join("native-session.json"),
        &json!({"pid": std::process::id(), "managed_session_id": "session-origin"}),
    );
    root.session("session-child", "ready", Some("session-origin"));
    let (_, check) = root.doctor_check("session-child");
    assert_eq!(
        (
            check["availability"].as_str(),
            check["reason_code"].as_str()
        ),
        (Some("available"), Some("origin_live"))
    );
    assert!(
        !root.status_of("session-child")["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("origin_ended"))
    );
    write(&origin.join("status.json"), &status("closed", 3, None));
    write(&origin.join("closed.json"), &status("closed", 3, None));
    let closed = root.session("session-closed-child", "closed", Some("session-origin"));
    write(&closed.join("closed.json"), &status("closed", 2, None));
    let (_, check) = root.doctor_check("session-closed-child");
    assert_eq!(
        (
            check["availability"].as_str(),
            check["reason_code"].as_str()
        ),
        (Some("available"), Some("origin_ended_session_closed"))
    );
    assert!(check["next_command"].is_null());
    // The status filter keeps its meaning: closed sessions carry no origin attention.
    assert!(
        !root.status_of("session-closed-child")["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("origin_ended"))
    );
}

#[test]
fn close_reports_unclosed_sessions_started_from_it_and_keeps_its_success() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    let child = root.session("session-child", "ready", Some("session-origin"));
    let done = root.session("session-done", "closed", Some("session-origin"));
    write(&done.join("closed.json"), &status("closed", 2, None));
    root.session("session-unrelated", "ready", Some("external"));
    let child_before = entries(&child);
    let output = root.run(&["close-session", "session-origin", "--explicit", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({"ok": true, "session": "session-origin", "closed": true, "started_from": {
            "open": ["session-child"], "incomplete": false, "incomplete_reasons": [],
            "additional_incomplete_reasons": 0, "scanned": {"sessions": 3, "listed": 1}}})
    );
    assert_eq!(entries(&child), child_before);
    // The ended origin is now visible from the session it started.
    assert_eq!(
        root.status_of("session-child")["origin_observation"]["reason"],
        "recorded_closed"
    );

    // A scan that cannot classify a session is incomplete; the close still succeeds.
    root.session("session-second", "failed", None);
    root.session("session-mine", "ready", Some("session-second"));
    let damaged = root.session("session-damaged", "ready", Some("session-second"));
    fs::write(damaged.join("requests/100-1.json"), b"{").unwrap();
    let output = root.run(&["close-session", "session-second", "--explicit"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text,
        "closed session-second\n\
         1 session(s) started from session-second remain unclosed in the observed records: session-mine\n\
         Close a session you no longer need with 'tabcli close-session ID --explicit'.\n\
         Started-from scan incomplete; the list may omit sessions.\n"
    );
}

// `--with-started-from`: the listed sessions are closed through their own ordinary close.
// The fixtures below need no terminal: a dead reaped owner, no owner at all, or a refusal
// that the ordinary close reports before any terminal automation.

fn reaped_child_pid() -> u32 {
    #[cfg(windows)]
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn owner(directory: &Path, id: &str, pid: u32) {
    write(
        &directory.join("native-session.json"),
        &json!({"pid": pid, "managed_session_id": id}),
    );
}

/// A handle-less `launching` session whose launch is still pending: the ordinary close
/// refuses it, and so does the flagged close.
fn pending_launch(directory: &Path) {
    write(
        &directory.join("status.json"),
        &status("launching", 2, None),
    );
    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 600_000;
    write(
        &directory.join("launch.json"),
        &json!({"schema": 1, "claim_token": "100-1", "deadline_unix_ms": deadline,
            "phase": "pending"}),
    );
}

fn without_lock(entries: Vec<support::Entry>) -> Vec<support::Entry> {
    entries
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name != "turn.claim.lock")
        })
        .collect()
}

fn report(output: &Output) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        stdout,
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
        "exactly one JSON object on stdout"
    );
    value
}

#[test]
fn with_started_from_closes_each_listed_session_through_its_own_close() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    // A dead, reaped owner and no handle: the ordinary dead-owner repair closes it.
    let dead = root.session("session-dead", "working", Some("session-origin"));
    fs::write(dead.join("turn.claim"), "100-1").unwrap();
    owner(&dead, "session-dead", reaped_child_pid());
    // No owner and no handle: the ordinary close records the cascade reason.
    let plain = root.session("session-plain", "ready", Some("session-origin"));
    // Its own started-from session is reported under it and never closed.
    let grand = root.session("session-grand", "working", Some("session-plain"));
    // A session that closed before the command is not a candidate.
    let done = root.session("session-done", "closed", Some("session-origin"));
    write(&done.join("closed.json"), &status("closed", 2, None));
    // A current Claim the scan could not read cannot be matched: skipped, never closed.
    let unreadable = root.session("session-unreadable", "ready", Some("session-origin"));
    fs::write(unreadable.join("turn.claim"), [0xff, 0xfe]).unwrap();
    root.session("session-unrelated", "ready", Some("external"));
    let grand_before = entries(&grand);
    let unreadable_before = entries(&unreadable);
    let done_before = entries(&done);

    let output = root.run(&[
        "close-session",
        "session-origin",
        "--explicit",
        "--with-started-from",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    // Reported, not closed: one level only. Compared before any query runs.
    assert_eq!(entries(&grand), grand_before);
    assert_eq!(entries(&unreadable), unreadable_before);
    assert_eq!(entries(&done), done_before);
    let value = report(&output);
    let scan = |sessions: u64, listed: u64, open: Value| {
        json!({"open": open, "incomplete": false, "incomplete_reasons": [],
            "additional_incomplete_reasons": 0,
            "scanned": {"sessions": sessions, "listed": listed}})
    };
    assert_eq!(
        value,
        json!({"ok": true, "session": "session-origin", "closed": true, "started_from": {
            "open": ["session-dead", "session-plain", "session-unreadable"],
            "incomplete": false, "incomplete_reasons": [],
            "additional_incomplete_reasons": 0, "scanned": {"sessions": 6, "listed": 3},
            "closed": [
                {"session": "session-dead", "started_from": scan(6, 0, json!([]))},
                {"session": "session-plain",
                    "started_from": scan(6, 1, json!(["session-grand"]))}],
            "skipped": [{"session": "session-unreadable", "reason": "session_changed: claim"}],
            "failed": []}})
    );
    // The dead-owner target closed through its repair and keeps the repair's diagnostic.
    let dead_status = root.sessions_of("session-dead");
    assert_eq!(dead_status["state"], "closed");
    let error = dead_status["error"].as_str().unwrap();
    assert!(error.contains("no longer running"), "{error}");
    assert!(!error.contains("--with-started-from"), "{error}");
    assert!(!dead.join("turn.claim").exists());
    assert!(dead.join("closed.json").exists());
    // The ownerless target keeps the supplied reason, visible as the session error.
    let inspect = success(root.run(&["inspect", "session-plain", "--json"]));
    assert_eq!(inspect["stored_state"], "closed");
    assert_eq!(
        inspect["error"],
        "closed by close-session --with-started-from of session-origin"
    );
    assert!(plain.join("closed.json").exists());
    assert_eq!(root.sessions_of("session-grand")["state"], "working");
    assert_eq!(root.sessions_of("session-unreadable")["state"], "ready");
    assert_eq!(root.sessions_of("session-unrelated")["state"], "ready");
}

#[test]
fn a_target_the_ordinary_close_refuses_fails_the_command_after_the_full_report() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    let plain = root.session("session-a-plain", "ready", Some("session-origin"));
    let launching = root.session("session-b-launching", "ready", Some("session-origin"));
    pending_launch(&launching);
    let launching_before = entries(&launching);

    let output = root.run(&[
        "close-session",
        "session-origin",
        "--explicit",
        "--with-started-from",
        "--json",
    ]);
    assert!(!output.status.success());
    let value = report(&output);
    assert_eq!(value["ok"], false);
    assert_eq!(value["closed"], true);
    assert_eq!(value["session"], "session-origin");
    assert_eq!(
        value["started_from"]["open"],
        json!(["session-a-plain", "session-b-launching"])
    );
    assert_eq!(
        value["started_from"]["closed"][0]["session"],
        "session-a-plain"
    );
    assert_eq!(value["started_from"]["skipped"], json!([]));
    let failed = &value["started_from"]["failed"];
    assert_eq!(failed.as_array().unwrap().len(), 1);
    assert_eq!(failed[0]["session"], "session-b-launching");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains("the launcher is still creating the surface for this session"),
        "{error}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "close-session closed session-origin but 1 of 2 started-from session(s) failed to close; see the report"
        ),
        "{stderr}"
    );
    assert!(plain.join("closed.json").exists());
    // The refused close took the session's lifecycle lock, as an explicit close of it
    // would, and changed nothing else.
    assert_eq!(without_lock(entries(&launching)), launching_before);
    assert_eq!(root.sessions_of("session-origin")["state"], "closed");
}

#[test]
fn the_human_report_lists_every_target_in_order_and_the_totals() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    let dead = root.session("session-a-dead", "working", Some("session-origin"));
    owner(&dead, "session-a-dead", reaped_child_pid());
    let launching = root.session("session-b-launching", "ready", Some("session-origin"));
    pending_launch(&launching);
    let plain = root.session("session-c-plain", "ready", Some("session-origin"));
    root.session("session-grand", "ready", Some("session-c-plain"));
    let unreadable = root.session("session-d-unreadable", "ready", Some("session-origin"));
    fs::write(unreadable.join("turn.claim"), [0xff, 0xfe]).unwrap();

    let output = root.run(&[
        "close-session",
        "session-origin",
        "--explicit",
        "--with-started-from",
    ]);
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 7, "{stdout}");
    assert_eq!(lines[0], "closed session-origin");
    assert_eq!(lines[1], "started-from candidates: 4 (scan complete)");
    assert_eq!(
        lines[2],
        "closed session-a-dead (started from session-origin); its own started-from: 0 open"
    );
    assert!(
        lines[3].starts_with(
            "failed session-b-launching: the launcher is still creating the surface for this session"
        ),
        "{}",
        lines[3]
    );
    assert_eq!(
        lines[4],
        "closed session-c-plain (started from session-origin); its own started-from: 1 open"
    );
    assert_eq!(
        lines[5],
        "skipped session-d-unreadable: session_changed: claim"
    );
    assert_eq!(
        lines[6],
        "started-from close: 2 closed, 1 skipped, 1 failed"
    );
    assert!(!stdout.contains("remain unclosed"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("but 1 of 4 started-from session(s) failed to close"),
        "{stderr}"
    );
    assert!(plain.join("closed.json").exists());
    assert_eq!(root.sessions_of("session-grand")["state"], "ready");
}

/// A retained, bound handle whose owner cannot be attested: the ordinary authority path
/// refuses before any terminal automation. On macOS a recorded birth with `pid: u32::MAX`
/// is the explicit range error; the Windows observer classifies that pid as absent, so no
/// Windows public case is claimed here.
#[cfg(target_os = "macos")]
#[test]
fn a_target_with_an_unknown_owner_and_a_retained_handle_is_failed() {
    let root = Root::new();
    root.session("session-origin", "failed", None);
    let unknown = root.session("session-unknown", "ready", Some("session-origin"));
    write(
        &unknown.join("terminal.json"),
        &json!({"terminal": "apple-terminal", "session_id": "/dev/ttys999",
            "window_id": "1001", "managed_session_id": "session-unknown"}),
    );
    write(
        &unknown.join("native-session.json"),
        &json!({"pid": u32::MAX, "managed_session_id": "session-unknown",
            "terminal_tty": "/dev/ttys999", "process_start_seconds": 1,
            "process_start_microseconds": 0}),
    );

    let output = root.run(&[
        "close-session",
        "session-origin",
        "--explicit",
        "--with-started-from",
        "--json",
    ]);
    assert!(!output.status.success());
    let value = report(&output);
    assert_eq!(value["ok"], false);
    assert_eq!(value["closed"], true);
    assert_eq!(value["started_from"]["open"], json!(["session-unknown"]));
    assert_eq!(value["started_from"]["closed"], json!([]));
    assert_eq!(value["started_from"]["skipped"], json!([]));
    assert_eq!(
        value["started_from"]["failed"][0]["session"],
        "session-unknown"
    );
    let error = value["started_from"]["failed"][0]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("out of range"), "{error}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "close-session closed session-origin but 1 of 1 started-from session(s) failed to close; see the report"
        ),
        "{stderr}"
    );
    // The refusal kept the handle and the session open.
    assert!(unknown.join("terminal.json").exists());
    assert!(!unknown.join("closed.json").exists());
    assert_eq!(root.sessions_of("session-unknown")["state"], "ready");
}
