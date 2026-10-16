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
