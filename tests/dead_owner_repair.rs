//! Dead-owner repair through the public entry paths (issue #5). `tell` and `sessions`
//! repair a session whose recorded native owner is no longer running; `result` never does.
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
}

impl Fixture {
    fn dead_owner() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-deadowner");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let workspace = root.path().canonicalize().unwrap();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": 1, "id": "session-deadowner", "provider": "codex",
                "provider_path": "codex", "provider_version": "0.147.0",
                "workspace": workspace, "title": "dead owner", "model": null,
                "effort": null, "yolo": false, "created_unix_ms": 1
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            directory.join("status.json"),
            serde_json::to_vec_pretty(&json!({
                "state": "working", "generation": 3, "updated_unix_ms": 2,
                "exit_code": null, "error": null
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(directory.join("turn.claim"), "123-456-0\n").unwrap();
        // No live process ever has this PID; the record predates process identities.
        fs::write(
            directory.join("native-session.json"),
            serde_json::to_vec_pretty(&json!({ "pid": u32::MAX })).unwrap(),
        )
        .unwrap();
        Self { root, directory }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .output()
            .unwrap()
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&fs::read(self.directory.join("status.json")).unwrap()).unwrap()
    }

    fn assert_repaired(&self) {
        let status = self.status();
        assert_eq!(status["state"], "closed");
        assert!(
            status["error"]
                .as_str()
                .is_some_and(|error| error.contains("no longer running")),
            "{status}"
        );
        assert!(!self.directory.join("turn.claim").exists());
        assert!(self.directory.join("closed.json").is_file());
        assert!(self.directory.join("events").is_dir());
    }
}

#[test]
fn tell_repairs_a_dead_owner_before_refusing_the_turn() {
    let fixture = Fixture::dead_owner();
    let output = fixture.run(&[
        "tell",
        "session-deadowner",
        "--prompt",
        "continue",
        "--timeout-secs",
        "5",
        "--json",
    ]);
    assert!(!output.status.success());
    fixture.assert_repaired();
}

#[test]
fn sessions_repairs_a_dead_owner_and_lists_it_closed() {
    let fixture = Fixture::dead_owner();
    let workspace = fixture.root.path().canonicalize().unwrap();
    let output = fixture.run(&[
        "sessions",
        "--workspace",
        workspace.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions: Value = serde_json::from_slice(&output.stdout).unwrap();
    let listed = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "session-deadowner")
        .expect("the repaired session is listed");
    assert_eq!(listed["state"], "closed");
    fixture.assert_repaired();
}

#[test]
fn result_wait_reports_a_dead_owner_without_repairing_it() {
    let fixture = Fixture::dead_owner();
    let requests = fixture.directory.join("requests");
    fs::create_dir_all(&requests).unwrap();
    fs::write(
        requests.join("123-456-0.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": 1, "request_id": "request-dead", "claim_token": "123-456-0",
            "event_file": "event-1.json", "created_unix_ms": 3
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fixture.status();
    let output = fixture.run(&[
        "result",
        "session-deadowner",
        "--request",
        "request-dead",
        "--wait",
        "--timeout-secs",
        "5",
        "--json",
    ]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["request_state"], "unresolved");
    assert_eq!(result["owner_process_alive"], false);
    assert_eq!(fixture.status(), before);
    assert!(fixture.directory.join("turn.claim").exists());
}
