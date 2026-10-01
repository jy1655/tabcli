use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-observe");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        write(
            &directory.join("manifest.json"),
            &json!({
                "schema": 1, "id": "session-observe", "provider": "claude",
                "provider_path": "claude", "provider_version": "2.1.280",
                "workspace": root.path().canonicalize().unwrap(), "title": "inspect fixture", "model": null,
                "effort": null, "yolo": false, "created_unix_ms": 1
            }),
        );
        write(
            &directory.join("status.json"),
            &json!({
                "state": "ready", "generation": 2, "updated_unix_ms": 2,
                "exit_code": null, "error": null
            }),
        );
        Self { root, directory }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .output()
            .unwrap()
    }

    fn event(&self, name: &str, text: &str) -> Value {
        let event = json!({ "provider": "claude", "message": text, "error": null,
            "provider_session_id": "native-session", "turn_id": name, "created_unix_ms": 3 });
        write(&self.directory.join("events").join(name), &event);
        event
    }
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            entries.extend(files(&entry.path()));
        } else {
            entries.push((entry.path(), fs::read(entry.path()).unwrap()));
        }
    }
    entries.sort();
    entries
}

#[test]
fn inspect_and_legacy_result_are_read_only_and_do_not_invent_request_identity() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "old result");
    let before = files(fixture.root.path());
    let inspected = success(fixture.run(&["inspect", "session-observe", "--json"]));
    assert_eq!(inspected["schema_version"], 1);
    let result = success(fixture.run(&[
        "result",
        "session-observe",
        "--event",
        "event-1.json",
        "--json",
    ]));
    assert_eq!(result["result"], "old result");
    assert_eq!(result["request_id"], Value::Null);
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn result_never_publishes_a_partial_completion_journal_or_recovers_it() {
    let fixture = Fixture::new();
    let event = fixture.event("event-1.json", "not committed yet");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    write(
        &fixture.directory.join("turn.completion.json"),
        &json!({
            "schema": 1, "claim_token": "123-456-0", "event_file": "event-1.json",
            "event": event, "status_error": null, "status_state": "ready"
        }),
    );
    let before = files(fixture.root.path());
    let result = fixture.run(&[
        "result",
        "session-observe",
        "--event",
        "event-1.json",
        "--json",
    ]);
    let body: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(body["result"], Value::Null);
    assert_ne!(body["request_state"], "completed");
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn result_rejects_conflicting_selectors_and_path_traversal() {
    let fixture = Fixture::new();
    for args in [
        vec!["result", "session-observe", "--latest", "--list"],
        vec!["result", "session-observe", "--event", "../status.json"],
        vec!["result", "session-observe", "--request", "../other"],
        vec!["result", "session-observe", "--list", "--wait"],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
}

fn request(fixture: &Fixture, claim: &str, id: &str, event: &str) {
    let directory = fixture.directory.join("requests");
    fs::create_dir_all(&directory).unwrap();
    write(
        &directory.join(format!("{claim}.json")),
        &json!({
            "schema": 1, "request_id": id, "claim_token": claim,
            "event_file": event, "created_unix_ms": 3
        }),
    );
}

#[test]
fn exact_request_lookup_survives_a_later_turn_and_identical_result_text() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    request(&fixture, "123-457-1", "request-second", "event-2.json");
    fixture.event("event-1.json", "identical result");
    fixture.event("event-2.json", "identical result");
    fs::write(fixture.directory.join("turn.claim"), "123-458-2\n").unwrap();
    write(
        &fixture.directory.join("status.json"),
        &json!({
            "state": "working", "generation": 5, "updated_unix_ms": 5,
            "exit_code": null, "error": null
        }),
    );
    let result = success(fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-first",
        "--json",
    ]));
    assert_eq!(result["request_id"], "request-first");
    assert_eq!(result["event_id"], "event-1.json");
    assert_eq!(result["turn_id"], "event-1.json");
    assert_eq!(result["request_state"], "completed");
}

#[test]
fn waiting_timeout_preserves_the_claim_and_receipt() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-pending", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    let before = files(fixture.root.path());
    let output = fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-pending",
        "--wait",
        "--timeout-secs",
        "1",
        "--json",
    ]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["request_id"], "request-pending");
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["result"], Value::Null);
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn unknown_request_does_not_fall_back_to_latest() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "unrelated result");
    let output = fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-missing",
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unrelated result"));
}

#[test]
fn wait_stops_with_nonzero_status_for_unresolved_and_failed_requests() {
    for failed_event in [false, true] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-ended", "event-1.json");
        if failed_event {
            let mut event = fixture.event("event-1.json", "provider failure details");
            event["error"] = json!("provider failed");
            write(&fixture.directory.join("events/event-1.json"), &event);
        }
        let started = std::time::Instant::now();
        let output = fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-ended",
            "--wait",
            "--timeout-secs",
            "60",
            "--json",
        ]);
        assert!(!output.status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(
            result["request_state"],
            if failed_event { "failed" } else { "unresolved" }
        );
        if failed_event {
            assert_eq!(result["error"], "provider failed");
        }
    }
}

#[test]
fn wait_deadline_is_respected_while_a_writer_holds_the_lifecycle_lock() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-busy", "event-1.json");
    let lock = fs::File::create(fixture.directory.join("turn.claim.lock")).unwrap();
    lock.lock().unwrap();
    let started = std::time::Instant::now();
    let output = fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-busy",
        "--wait",
        "--timeout-secs",
        "1",
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["request_state"], "busy");
    assert_eq!(result["timed_out"], true);
}

#[test]
fn read_only_queries_preserve_closed_and_dead_owner_records() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "preserved result");
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid": u32::MAX}),
    );
    write(
        &fixture.directory.join("closed.json"),
        &json!({"state": "closed", "generation": 3, "updated_unix_ms": 4, "exit_code": null, "error": null}),
    );
    let before = files(fixture.root.path());
    success(fixture.run(&["inspect", "session-observe", "--json"]));
    success(fixture.run(&["result", "session-observe", "--latest", "--json"]));
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn result_reports_corrupt_events_instead_of_an_empty_success() {
    let fixture = Fixture::new();
    fs::write(fixture.directory.join("events/event-1.json"), "not JSON").unwrap();
    let output = fixture.run(&["result", "session-observe", "--latest", "--json"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("invalid JSON"));
}

#[test]
fn sessions_filters_and_updated_sort_preserve_the_array_contract() {
    let fixture = Fixture::new();
    let other = fixture.root.path().join("session-newer");
    fs::create_dir(&other).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("manifest.json")).unwrap())
            .unwrap();
    manifest["id"] = json!("session-newer");
    manifest["provider"] = json!("codex");
    write(&other.join("manifest.json"), &manifest);
    write(
        &other.join("status.json"),
        &json!({"state":"working","generation":4,"updated_unix_ms":20,"exit_code":null,"error":null}),
    );
    let output = success(fixture.run(&["sessions", "--sort", "updated", "--json"]));
    assert_eq!(output[0]["id"], "session-newer");
    let workspace = fixture.root.path().to_str().unwrap();
    let filtered = success(fixture.run(&[
        "sessions",
        "--workspace",
        workspace,
        "--provider",
        "claude",
        "--state",
        "ready",
        "--json",
    ]));
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["id"], "session-observe");
}

#[test]
fn corrupt_receipt_is_reported_without_hiding_an_independently_addressed_result() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-broken", "event-1.json");
    fixture.event("event-1.json", "verified provider result");
    fs::write(fixture.directory.join("requests/123-456-0.json"), "broken").unwrap();
    let before = files(fixture.root.path());
    let inspected = success(fixture.run(&["inspect", "session-observe", "--json"]));
    assert_eq!(inspected["unreadable_requests"], 1);
    let result = success(fixture.run(&[
        "result",
        "session-observe",
        "--event",
        "event-1.json",
        "--json",
    ]));
    assert_eq!(result["result"], "verified provider result");
    assert_eq!(result["request_id"], Value::Null);
    assert_eq!(result["unreadable_requests"], 1);
    assert!(
        !fixture
            .run(&[
                "result",
                "session-observe",
                "--request",
                "request-broken",
                "--json"
            ])
            .status
            .success()
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn dead_owner_ends_wait_without_repairing_or_releasing_the_claim() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-dead", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid": u32::MAX}),
    );
    let before = files(fixture.root.path());
    let started = std::time::Instant::now();
    let output = fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-dead",
        "--wait",
        "--timeout-secs",
        "10",
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["owner_process_alive"], false);
    assert_eq!(result["timed_out"], Value::Null);
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn empty_latest_is_a_structured_nonzero_result_but_inspect_still_works() {
    let fixture = Fixture::new();
    let output = fixture.run(&["result", "session-observe", "--latest", "--json"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["request_state"], "unavailable");
    assert_eq!(result["ok"], false);
    success(fixture.run(&["inspect", "session-observe", "--json"]));
}

#[test]
fn inspect_reports_unreadable_status_without_repairing_it() {
    let fixture = Fixture::new();
    fs::write(fixture.directory.join("status.json"), "broken status").unwrap();
    let before = files(fixture.root.path());
    let output = fixture.run(&["inspect", "session-observe", "--json"]);
    assert!(!output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["stored_state"], "unknown");
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn result_human_output_reports_bridge_elapsed_and_legacy_reason() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "old result");
    let human = |args: &[&str]| {
        let output = fixture.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(
        human(&["result", "session-observe", "--latest"])
            .contains("Bridge observed elapsed: not computable (missing_receipt)")
    );
    fs::create_dir(fixture.directory.join("requests")).unwrap();
    write(
        &fixture.directory.join("requests/1-2-3.json"),
        &json!({
            "schema": 1, "request_id": "request-elapsed", "claim_token": "1-2-3",
            "event_file": "event-1.json", "created_unix_ms": 1
        }),
    );
    assert!(
        human(&["result", "session-observe", "--request", "request-elapsed"])
            .contains("Bridge observed elapsed: 2 ms")
    );
    assert!(
        human(&["result", "session-observe", "--list"]).contains("Bridge observed elapsed: 2 ms")
    );
}
