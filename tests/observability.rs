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
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
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
fn inspect_reports_typed_and_legacy_residual_surfaces_without_writing() {
    for (typed, error, residual) in [
        (None, "ordinary error", false),
        (
            None,
            "the surface may remain and is not closed by Bridge",
            true,
        ),
        (
            Some("unverified"),
            "wording changed without clearing the observation",
            true,
        ),
        (
            Some("cleared"),
            "the surface may remain and is not closed by Bridge",
            false,
        ),
    ] {
        let fixture = Fixture::new();
        let mut status = json!({"state":"closed", "generation":3,
            "updated_unix_ms":4, "exit_code":null, "error":error});
        if let Some(typed) = typed {
            status["residual_surface"] = json!(typed);
        }
        write(&fixture.directory.join("status.json"), &status);
        let before = files(fixture.root.path());
        let inspected = success(fixture.run(&["inspect", "session-observe", "--json"]));
        assert_eq!(inspected["stored_state"], "closed");
        assert_eq!(inspected["error"], error);
        assert_eq!(
            inspected["residual_surface"],
            if residual {
                json!("unverified")
            } else {
                Value::Null
            }
        );
        assert_eq!(files(fixture.root.path()), before);
    }
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

fn timeline(fixture: &Fixture, request: Option<&str>) -> Value {
    let mut args = vec!["inspect", "session-observe", "--timeline", "--json"];
    if let Some(request) = request {
        args.extend(["--request", request]);
    }
    success(fixture.run(&args))
}

#[test]
fn timeline_preserves_identity_closed_results_and_every_session_byte() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    request(&fixture, "123-457-1", "request-second", "event-2.json");
    fixture.event("event-1.json", "identical result");
    fixture.event("event-2.json", "identical result");
    fixture.event("event-0.json", "legacy result");
    write(
        &fixture.directory.join("launch.json"),
        &json!({"schema": 1,
        "claim_token": "123-456-0", "phase": "spawned", "deadline_unix_ms": 999}),
    );
    fs::write(
        fixture.directory.join("launch.log"),
        "5 arbitrary free text\nno timestamp\n1 failed sent ready\n",
    )
    .unwrap();
    let status = json!({"state": "closed", "generation": 6, "updated_unix_ms": 6,
        "exit_code": null, "error": "explicit close"});
    write(&fixture.directory.join("status.json"), &status);
    write(&fixture.directory.join("closed.json"), &status);
    write(
        &fixture.directory.join("terminal.closed.json"),
        &json!({"consumed": true, "terminal": "windows-console"}),
    );
    fs::write(fixture.directory.join("turn.claim.lock"), "").unwrap();
    let before = files(fixture.root.path());
    let all = timeline(&fixture, None);
    assert_eq!(all["requests"].as_array().unwrap().len(), 2);
    for (index, id) in ["request-first", "request-second"].iter().enumerate() {
        assert_eq!(all["requests"][index]["request_id"], *id);
        assert_eq!(all["requests"][index]["request_state"], "completed");
        assert_eq!(
            all["requests"][index]["turn_id"],
            format!("event-{}.json", index + 1)
        );
    }
    let selected = timeline(&fixture, Some("request-first"));
    assert_eq!(selected["requests"].as_array().unwrap().len(), 1);
    let entries = selected["entries"].as_array().unwrap();
    assert!(entries.iter().all(|e| e["request_id"] == "request-first"));
    let phase = entries
        .iter()
        .find(|e| e["stage"] == "launch_phase")
        .unwrap();
    assert_eq!(phase["observed_unix_ms"], Value::Null);
    assert_eq!(phase["detail"]["phase"], "spawned");
    let session = selected["session_entries"].as_array().unwrap();
    assert!(session.iter().all(|e| e["request_id"].is_null()));
    assert!(session.iter().any(|e| e["event_id"] == "event-0.json"));
    assert!(
        session
            .iter()
            .any(|e| e["stage"] == "closed_status" && e["observed_unix_ms"] == 6)
    );
    assert!(
        session
            .iter()
            .any(|e| e["stage"] == "terminal_closed" && e["observed_unix_ms"].is_null())
    );
    let logs = session
        .iter()
        .filter(|e| e["stage"] == "launch_log")
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0]["detail"]["text"], "1 failed sent ready");
    assert_eq!(logs[0]["stage"], "launch_log");
    assert_eq!(logs[2]["observed_unix_ms"], Value::Null);
    assert_eq!(logs[2]["detail"]["line"], 2);
    assert_eq!(
        selected["doctor_command"],
        "tabcli doctor session-observe --json"
    );
    let human = fixture.run(&[
        "inspect",
        "session-observe",
        "--timeline",
        "--request",
        "request-first",
    ]);
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("time=unknown"));
    assert!(human.contains("source=\"launch.json\""));
    assert!(human.contains("session_entries:"));
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn timeline_states_match_exact_result_and_never_confirm_uncertain_delivery() {
    for state in [
        "pending",
        "failed",
        "unresolved",
        "recovery_required",
        "completed",
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-turn", "event-1.json");
        if state == "pending" || state == "recovery_required" {
            fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
        }
        if state == "failed" || state == "completed" {
            let mut event = fixture.event("event-1.json", "result");
            if state == "failed" {
                event["error"] = json!("provider failed");
            }
            write(&fixture.directory.join("events/event-1.json"), &event);
        }
        if state == "recovery_required" {
            let event = fixture.event("event-1.json", "unpublished bytes");
            let mut proposed = event;
            proposed["message"] = json!("different journal bytes");
            write(
                &fixture.directory.join("turn.completion.json"),
                &json!({"schema": 1,
                "claim_token": "123-456-0", "event_file": "event-1.json", "event": proposed,
                "status_error": null, "status_state": "ready"}),
            );
        }
        write(
            &fixture.directory.join("status.json"),
            &json!({"state": "working", "generation": 3,
            "updated_unix_ms": 4, "exit_code": null, "error": "delivery could not be confirmed; do not resend"}),
        );
        let before = files(fixture.root.path());
        let value = timeline(&fixture, Some("request-turn"));
        let exact = success(fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-turn",
            "--json",
        ]));
        assert_eq!(
            value["requests"][0]["request_state"],
            exact["request_state"]
        );
        assert_eq!(value["requests"][0]["request_state"], state);
        assert_eq!(value["session_error"], exact["session_error"]);
        if state == "pending" || state == "unresolved" || state == "recovery_required" {
            assert_eq!(value["requests"][0]["delivery"], "unknown");
            assert!(
                !value["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["stage"] == "completion" && e["record_state"] == "observed")
            );
        }
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn timeline_reports_missing_and_damaged_evidence_without_success() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-turn", "event-1.json");
    let missing = timeline(&fixture, None);
    assert!(
        missing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == "events/event-1.json" && e["record_state"] == "missing")
    );
    fs::write(fixture.directory.join("events/event-1.json"), "{broken").unwrap();
    fs::write(fixture.directory.join("requests/123-457-1.json"), "{broken").unwrap();
    fs::write(fixture.directory.join("closed.json"), "{broken").unwrap();
    fs::write(fixture.directory.join("terminal.closing.json"), "{broken").unwrap();
    let before = files(fixture.root.path());
    let damaged = timeline(&fixture, None);
    assert_eq!(damaged["incomplete"], true);
    assert_eq!(damaged["unreadable_requests"], 1);
    assert_eq!(damaged["requests"][0]["request_state"], "unknown");
    assert_eq!(damaged["requests"][0]["delivery"], "unknown");
    assert!(
        damaged["session_entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["stage"] == "closed_status" && e["record_state"] == "unreadable")
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn timeline_rejects_unknown_requests_invalid_options_and_corrupt_core_records() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "unrelated result");
    for args in [
        vec!["inspect", "session-observe", "--request", "request-one"],
        vec![
            "inspect",
            "session-observe",
            "--timeline",
            "--request",
            "../outside",
        ],
        vec!["inspect", "session-observe", "--timeline", "--timeline"],
        vec!["inspect", "session-observe", "--timeline", "--request"],
        vec!["inspect", "session-observe", "--timeline", "--wait"],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
    let output = fixture.run(&[
        "inspect",
        "session-observe",
        "--timeline",
        "--request",
        "request-missing",
        "--json",
    ]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unrelated result"));
    fs::write(fixture.directory.join("launch.json"), "{broken").unwrap();
    let before = files(fixture.root.path());
    let output = fixture.run(&["inspect", "session-observe", "--timeline", "--json"]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["stored_state"], "unknown");
    assert_eq!(files(fixture.root.path()), before);
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

#[test]
fn timeline_invalid_utf8_is_unreadable() {
    for source in [
        "events/event-1.json",
        "requests/123-456-0.json",
        "closed.json",
        "terminal.closed.json",
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-turn", "event-1.json");
        fixture.event("event-1.json", "damage");
        if source == "closed.json" {
            fs::copy(
                fixture.directory.join("status.json"),
                fixture.directory.join(source),
            )
            .unwrap();
        }
        if source == "terminal.closed.json" {
            write(
                &fixture.directory.join(source),
                &json!({"message": "damage"}),
            );
        }
        let path = fixture.directory.join(source);
        let mut bytes = fs::read(&path).unwrap();
        let needle: &[u8] = if source.starts_with("requests/") {
            b"request-turn"
        } else if source == "closed.json" {
            b"ready"
        } else {
            b"damage"
        };
        let at = bytes
            .windows(needle.len())
            .position(|b| b == needle)
            .unwrap();
        bytes[at] = 255;
        fs::write(path, bytes).unwrap();
        let value = timeline(&fixture, None);
        assert!(
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .chain(value["session_entries"].as_array().unwrap())
                .any(|e| e["source"] == source && e["record_state"] == "unreadable"),
            "{source}: {value}"
        );
    }
}

#[test]
fn timeline_oversized_records_are_unreadable() {
    for (source, limit) in [
        ("launch.log", 1024 * 1024),
        ("events/event-1.json", 64 * 1024 * 1024),
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-turn", "event-1.json");
        let file = fs::File::create(fixture.directory.join(source)).unwrap();
        file.set_len(limit + 1).unwrap();
        let value = timeline(&fixture, None);
        let e = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .chain(value["session_entries"].as_array().unwrap())
            .find(|e| e["source"] == source)
            .unwrap();
        assert_eq!(e["record_state"], "unreadable");
        assert!(e["detail"].to_string().contains(&limit.to_string()));
    }
}

#[test]
fn timeline_log_timestamp_overflow_has_unknown_time() {
    let fixture = Fixture::new();
    fs::write(
        fixture.directory.join("launch.log"),
        "18446744073709551616 message\n",
    )
    .unwrap();
    let value = timeline(&fixture, None);
    let e = value["session_entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stage"] == "launch_log")
        .unwrap();
    assert!(e["observed_unix_ms"].is_null());
    assert_eq!(e["detail"]["text"], "18446744073709551616 message");
}

#[test]
fn timeline_completion_without_time_is_recorded_delivery() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-turn", "event-1.json");
    let mut event = fixture.event("event-1.json", "done");
    event["created_unix_ms"] = Value::Null;
    write(&fixture.directory.join("events/event-1.json"), &event);
    let value = timeline(&fixture, Some("request-turn"));
    assert_eq!(value["requests"][0]["request_state"], "completed");
    assert_eq!(value["requests"][0]["delivery"], "completion_recorded");
    assert!(
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["stage"] == "completion")
            .unwrap()["observed_unix_ms"]
            .is_null()
    );
}

#[test]
fn timeline_expired_launch_summary_is_derived_from_result() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-turn", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    write(
        &fixture.directory.join("status.json"),
        &json!({"state": "launching", "updated_unix_ms": 4}),
    );
    write(
        &fixture.directory.join("launch.json"),
        &json!({"schema":1,"claim_token":"123-456-0","phase":"pending","deadline_unix_ms":1}),
    );
    let value = timeline(&fixture, Some("request-turn"));
    let exact = success(fixture.run(&[
        "result",
        "session-observe",
        "--request",
        "request-turn",
        "--json",
    ]));
    assert_eq!(
        value["requests"][0]["request_state"],
        exact["request_state"]
    );
    assert_eq!(exact["request_state"], "unresolved");
    assert_eq!(value["requests"][0]["error"], exact["error"]);
    assert_eq!(value["requests"][0]["derived_from"], "result");
    let diagnostic = exact["error"].as_str().unwrap();
    assert!(!value["entries"].to_string().contains(diagnostic));
    let launch = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stage"] == "launch_phase")
        .unwrap();
    assert_eq!(
        launch["detail"],
        json!({"phase":"pending","deadline_unix_ms":1})
    );
    assert!(launch["observed_unix_ms"].is_null());
}

#[test]
fn timeline_request_ignores_other_requests_event_bytes() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    request(&fixture, "123-457-1", "request-second", "event-2.json");
    fixture.event("event-1.json", "done");
    fs::write(fixture.directory.join("events/event-2.json"), [255]).unwrap();
    let selected = timeline(&fixture, Some("request-first"));
    assert_eq!(selected["incomplete"], false);
    assert!(!selected.to_string().contains("events/event-2.json"));
    assert_eq!(timeline(&fixture, None)["incomplete"], true);
}

#[test]
fn timeline_recorded_timestamp_overflow_never_panics() {
    for source in [
        "events/event-1.json",
        "requests/123-456-0.json",
        "status.json",
        "launch.json",
        "closed.json",
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-turn", "event-1.json");
        fixture.event("event-1.json", "done");
        if source == "closed.json" {
            fs::copy(
                fixture.directory.join("status.json"),
                fixture.directory.join(source),
            )
            .unwrap();
        }
        if source == "launch.json" {
            fs::write(
                fixture.directory.join(source),
                r#"{"schema":1,"claim_token":"123-456-0","phase":"pending","deadline_unix_ms":3}"#,
            )
            .unwrap();
        }
        let path = fixture.directory.join(source);
        let text = fs::read_to_string(&path).unwrap();
        let field = if source == "launch.json" {
            "deadline_unix_ms"
        } else if source.ends_with("status.json") || source == "closed.json" {
            "updated_unix_ms"
        } else {
            "created_unix_ms"
        };
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value[field] = json!("OVERFLOW");
        let text = serde_json::to_string(&value)
            .unwrap()
            .replace("\"OVERFLOW\"", "18446744073709551616");
        fs::write(path, text).unwrap();
        let output = fixture.run(&["inspect", "session-observe", "--timeline", "--json"]);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            value["ok"] == false || value["incomplete"] == true,
            "{source}: {value}"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn timeline_unpublished_invalid_utf8_is_still_unreadable() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-turn", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    fs::write(fixture.directory.join("events/event-1.json"), [255]).unwrap();
    let value = timeline(&fixture, Some("request-turn"));
    let e = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stage"] == "completion")
        .unwrap();
    assert_eq!(e["record_state"], "unreadable");
    assert!(e["detail"].as_str().unwrap().contains("invalid UTF-8"));
    assert_eq!(value["requests"][0]["request_state"], "pending");
}

#[test]
fn timeline_launch_failure_without_completion_keeps_delivery_unknown() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-turn", "event-1.json");
    write(
        &fixture.directory.join("status.json"),
        &json!({"state":"failed","updated_unix_ms":4,"error":"launch failed"}),
    );
    write(
        &fixture.directory.join("launch.json"),
        &json!({"schema":1,"claim_token":"123-456-0","phase":"pending","deadline_unix_ms":1}),
    );
    let value = timeline(&fixture, Some("request-turn"));
    assert_eq!(value["requests"][0]["request_state"], "failed");
    assert_eq!(value["requests"][0]["delivery"], "unknown");
}

fn status_session(
    fixture: &Fixture,
    id: &str,
    state: &str,
    updated: u64,
    provider: &str,
    workspace: &Path,
) -> PathBuf {
    let directory = fixture.root.path().join(id);
    fs::create_dir_all(directory.join("events")).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("manifest.json")).unwrap())
            .unwrap();
    manifest["id"] = json!(id);
    manifest["provider"] = json!(provider);
    manifest["workspace"] = json!(workspace);
    write(&directory.join("manifest.json"), &manifest);
    write(
        &directory.join("status.json"),
        &json!({"state":state,"generation":1,
        "updated_unix_ms":updated,"exit_code":null,"error":null}),
    );
    directory
}

#[test]
fn status_is_read_only_including_closed_tombstones_and_dead_owners() {
    let fixture = Fixture::new();
    fixture.event("event-1.json", "preserved");
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":0,"managed_session_id":"session-observe"}),
    );
    let closed = status_session(
        &fixture,
        "session-closed",
        "closed",
        4,
        "codex",
        fixture.root.path(),
    );
    write(
        &closed.join("closed.json"),
        &json!({"state":"closed","generation":2,"updated_unix_ms":5,
        "exit_code":null,"error":null}),
    );
    write(
        &closed.join("status.json"),
        &json!({"state":"closed","generation":2,"updated_unix_ms":4,
        "exit_code":null,"error":null,"residual_surface":"unverified"}),
    );
    write(
        &closed.join("native-session.json"),
        &json!({"pid":0,"managed_session_id":"session-closed"}),
    );
    let before = files(fixture.root.path());
    let value = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(value["sessions"][0]["attention"], json!(["owner_exited"]));
    assert_eq!(
        value["sessions"][0]["latest_result"]["request_id"],
        Value::Null
    );
    assert_eq!(
        value["sessions"][0]["result_command"],
        "tabcli result session-observe --event event-1.json --json"
    );
    assert_eq!(files(fixture.root.path()), before);
    let value = success(fixture.run(&["status", "--all-workspaces", "--all", "--json"]));
    assert_eq!(value["scanned"], json!({"sessions":2,"listed":2}));
    let closed = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "session-closed")
        .unwrap();
    assert_eq!(closed["attention"], json!(["residual_surface_unverified"]));
    assert_eq!(closed["residual_surface"], "unverified");
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn status_scopes_before_observation_and_filters_provider() {
    let fixture = Fixture::new();
    let other_workspace = tempfile::tempdir().unwrap();
    let other_path = other_workspace.path().canonicalize().unwrap();
    let other = status_session(&fixture, "session-other", "exited", 4, "codex", &other_path);
    let run_here = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .current_dir(fixture.root.path())
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", fixture.root.path())
            .output()
            .unwrap()
    };
    let value = success(run_here(&["status", "--json"]));
    assert_eq!(
        value["filters"]["workspace"],
        json!(fixture.root.path().canonicalize().unwrap())
    );
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":1}));
    let value = success(fixture.run(&[
        "status",
        "--workspace",
        other_path.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(value["sessions"][0]["id"], "session-other");
    assert_eq!(value["sessions"][0]["state"], "exited");
    let value = success(fixture.run(&[
        "status",
        "--all-workspaces",
        "--provider",
        "codex",
        "--json",
    ]));
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":1}));
    let value = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert_eq!(value["scanned"], json!({"sessions":2,"listed":2}));
    fs::write(other.join("status.json"), "{").unwrap();
    let value = success(run_here(&["status", "--json"]));
    assert_eq!(value["incomplete"], false);
    let output = run_here(&["status", "--workspace", ".", "--all-workspaces", "--json"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["ok"], false);
    assert!(error["error"].as_str().unwrap().contains("only one"));
}

#[test]
fn status_sorts_attention_then_updated_then_id_and_includes_failed_by_default() {
    let fixture = Fixture::new();
    // A bound, live process gives a liveness fact even on hosts without identity observations.
    write(
        &fixture.directory.join("native-session.json"),
        &json!({"pid":std::process::id(),"managed_session_id":"session-observe"}),
    );
    for (id, state, updated) in [
        ("session-a", "ready", 10),
        ("session-b", "failed", 10),
        ("session-new", "ready", 20),
    ] {
        status_session(&fixture, id, state, updated, "codex", fixture.root.path());
    }
    let before = files(fixture.root.path());
    let value = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert_eq!(files(fixture.root.path()), before);
    let ids = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["session-new", "session-a", "session-b", "session-observe"]
    );
    assert!(
        value["sessions"][2]["attention"]
            .as_array()
            .unwrap()
            .contains(&json!("session_failed"))
    );
    assert!(
        value["sessions"][3]["attention"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn status_omits_unreadable_required_records_with_session_reasons() {
    let fixture = Fixture::new();
    fs::write(fixture.directory.join("status.json"), "{").unwrap();
    let before = files(fixture.root.path());
    let value = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert_eq!(value["sessions"], json!([]));
    assert_eq!(value["incomplete"], true);
    assert_eq!(value["incomplete_reasons"][0]["session"], "session-observe");
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn status_returns_busy_incomplete_without_writing_or_waiting_for_the_writer() {
    let fixture = Fixture::new();
    let lock = fs::File::create(fixture.directory.join("turn.claim.lock")).unwrap();
    // Snapshot the bytes before locking: Windows refuses to read a file that another
    // handle holds exclusively, and the lock file is part of the directory.
    let before = files(fixture.root.path());
    lock.lock().unwrap();
    let started = std::time::Instant::now();
    let value = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(value["scanned"], json!({"sessions":1,"listed":0}));
    assert_eq!(value["incomplete_reasons"][0]["session"], "session-observe");
    assert!(
        value["incomplete_reasons"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("busy")
    );
    drop(lock);
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn status_argument_errors_are_json_and_empty_human_output_succeeds() {
    let fixture = Fixture::new();
    for args in [
        vec!["status", "--bad", "--json"],
        vec!["status", "--provider", "unknown", "--json"],
        vec!["status", "--all", "--all", "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["sessions"], json!([]));
    }
    let output = fixture.run(&["status", "--all-workspaces", "--provider", "pi"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "no sessions to show\n0 listed / 0 scanned\n"
    );
}

#[test]
fn status_root_errors_are_structured_and_missing_roots_are_empty() {
    let fixture = Fixture::new();
    let not_directory = fixture.root.path().join("not-directory");
    fs::write(&not_directory, "not a directory").unwrap();
    let run = |root: &Path| {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(["status", "--all-workspaces", "--json"])
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root)
            .output()
            .unwrap()
    };
    let output = run(&not_directory);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    let missing = fixture.root.path().join("missing");
    let value = success(run(&missing));
    assert_eq!(value["scanned"], json!({"sessions":0,"listed":0}));
    assert!(!missing.exists());
}

#[test]
fn status_human_strings_cannot_inject_terminal_controls() {
    let fixture = Fixture::new();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("manifest.json")).unwrap())
            .unwrap();
    manifest["provider"] = json!("codex\n\u{1b}[31m");
    write(&fixture.directory.join("manifest.json"), &manifest);
    let output = fixture.run(&["status", "--all-workspaces"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("codex\\n\\u{1b}[31m"));
    assert!(!text.contains('\u{1b}'));
}

fn wait_second_session(fixture: &Fixture) -> PathBuf {
    let directory = fixture.root.path().join("session-second");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("manifest.json")).unwrap())
            .unwrap();
    manifest["id"] = json!("session-second");
    write(&directory.join("manifest.json"), &manifest);
    fs::copy(
        fixture.directory.join("status.json"),
        directory.join("status.json"),
    )
    .unwrap();
    fs::create_dir(directory.join("requests")).unwrap();
    write(
        &directory.join("requests/123-457-0.json"),
        &json!({
            "schema": 1, "request_id": "request-second", "claim_token": "123-457-0",
            "event_file": "event-2.json", "created_unix_ms": 3
        }),
    );
    fs::write(directory.join("turn.claim"), "123-457-0\n").unwrap();
    directory
}

#[test]
fn wait_selects_completed_and_preserves_other_claim_and_all_records() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    fixture.event("event-1.json", "first result");
    wait_second_session(&fixture);
    let before = files(fixture.root.path());
    let value = success(fixture.run(&[
        "wait",
        "session-observe/request-first",
        "session-second/request-second",
        "--json",
    ]));
    assert_eq!(value["ended"]["address"], "session-observe/request-first");
    assert_eq!(value["ended"]["result"], "first result");
    assert_eq!(value["remaining"], json!(["session-second/request-second"]));
    assert_eq!(value["timed_out"], false);
    assert!(value["ended"].get("owner").is_none());
    let human = fixture.run(&[
        "wait",
        "session-observe/request-first",
        "session-second/request-second",
    ]);
    assert!(human.status.success());
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .ends_with("remaining: session-second/request-second\n")
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn wait_shared_timeout_preserves_both_pending_requests() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    wait_second_session(&fixture);
    let before = files(fixture.root.path());
    let output = fixture.run(&[
        "wait",
        "session-observe/request-first",
        "session-second/request-second",
        "--timeout-secs",
        "1",
        "--json",
    ]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({"schema_version": 1, "ok": false, "ended": null,
        "remaining": ["session-observe/request-first", "session-second/request-second"],
        "timed_out": true, "error": "waiting timed out; no request was cancelled or resent"})
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn wait_ties_follow_address_order_instead_of_session_group_order() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
    request(&fixture, "123-458-0", "request-third", "event-3.json");
    fixture.event("event-3.json", "third");
    let second = wait_second_session(&fixture);
    let event = fixture.event("event-2.json", "second");
    write(&second.join("events/event-2.json"), &event);
    fs::remove_file(second.join("turn.claim")).unwrap();
    let value = success(fixture.run(&[
        "wait",
        "session-observe/request-first",
        "session-second/request-second",
        "session-observe/request-third",
        "--json",
    ]));
    assert_eq!(value["ended"]["address"], "session-second/request-second");
    assert_eq!(
        value["remaining"],
        json!([
            "session-observe/request-first",
            "session-observe/request-third"
        ])
    );
}

#[test]
fn wait_busy_unconfirmed_session_does_not_block_a_completed_request() {
    let fixture = Fixture::new();
    request(&fixture, "123-456-0", "request-first", "event-1.json");
    fixture.event("event-1.json", "completed");
    let second = wait_second_session(&fixture);
    let lock = fs::File::create(second.join("turn.claim.lock")).unwrap();
    let before = files(fixture.root.path());
    lock.lock().unwrap();
    // Even a missing receipt cannot be confirmed while this session is busy.
    let value = success(fixture.run(&[
        "wait",
        "session-second/request-missing",
        "session-observe/request-first",
        "--timeout-secs",
        "1",
        "--json",
    ]));
    lock.unlock().unwrap();
    assert_eq!(value["ended"]["address"], "session-observe/request-first");
    assert_eq!(
        value["remaining"],
        json!(["session-second/request-missing"])
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn wait_rejects_invalid_addresses_before_selecting_a_completed_request() {
    for invalid in [
        "session-second/request-missing",
        "session-missing/request-missing",
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-first", "event-1.json");
        fixture.event("event-1.json", "must not be returned");
        wait_second_session(&fixture);
        let before = files(fixture.root.path());
        let started = std::time::Instant::now();
        let output = fixture.run(&["wait", "session-observe/request-first", invalid, "--json"]);
        assert!(!output.status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ended"], Value::Null);
        assert_eq!(value["ok"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("must not be returned"));
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn wait_argument_errors_are_structured_json() {
    let fixture = Fixture::new();
    for args in [
        vec![],
        vec!["session-observe/latest"],
        vec!["session-observe/event-1.json"],
        vec!["../request-first"],
        vec!["session-observe/request-first/extra"],
        vec!["session-observe\\request-first"],
        vec![
            "session-observe/request-first",
            "session-observe/request-first",
        ],
        vec!["session-observe/request-first", "--timeout-secs", "0"],
        vec!["session-observe/request-first", "--timeout-secs"],
        vec!["--latest"],
        vec!["--json"],
    ] {
        let mut command = vec!["wait", "--json"];
        command.extend(args);
        let output = fixture.run(&command);
        assert!(!output.status.success(), "{command:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].is_string());
    }
}

#[test]
fn wait_one_matches_public_result_wait_and_preserves_journal_and_dead_owner() {
    for state in [
        "completed",
        "failed",
        "unresolved",
        "dead",
        "closed",
        "recovery_required",
        "published",
    ] {
        let fixture = Fixture::new();
        request(&fixture, "123-456-0", "request-first", "event-1.json");
        if matches!(state, "dead" | "closed" | "published" | "recovery_required") {
            fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
        }
        if matches!(
            state,
            "completed" | "failed" | "published" | "recovery_required"
        ) {
            let mut event = fixture.event("event-1.json", "result");
            if state == "failed" {
                event["error"] = json!("provider failed");
                write(&fixture.directory.join("events/event-1.json"), &event);
            }
            if matches!(state, "published" | "recovery_required") {
                if state == "recovery_required" {
                    event["message"] = json!("not published");
                }
                if state == "published" {
                    // Publication compares bytes with SessionEvent's writer order.
                    fs::write(
                        fixture.directory.join("events/event-1.json"),
                        concat!(
                            "{\n  \"provider\": \"claude\",\n  \"message\": \"result\",",
                            "\n  \"error\": null,\n  \"provider_session_id\": \"native-session\",",
                            "\n  \"turn_id\": \"event-1.json\",\n  \"created_unix_ms\": 3\n}"
                        ),
                    )
                    .unwrap();
                }
                write(
                    &fixture.directory.join("turn.completion.json"),
                    &json!({"schema": 1,
                    "claim_token": "123-456-0", "event_file": "event-1.json", "event": event,
                    "status_error": null, "status_state": "ready"}),
                );
            }
        }
        if matches!(state, "dead" | "completed") {
            write(
                &fixture.directory.join("native-session.json"),
                &json!({"pid": u32::MAX}),
            );
        }
        if state == "closed" {
            write(
                &fixture.directory.join("status.json"),
                &json!({"state": "closed", "generation": 3,
                "updated_unix_ms": 4, "exit_code": null, "error": null}),
            );
        }
        let before = files(fixture.root.path());
        let result = fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-first",
            "--wait",
            "--json",
        ]);
        let waited = fixture.run(&["wait", "session-observe/request-first", "--json"]);
        assert_eq!(result.status.code(), waited.status.code(), "{state}");
        let mut value: Value = serde_json::from_slice(&waited.stdout).unwrap();
        let top_ok = value["ok"].clone();
        let top_timed_out = value["timed_out"].clone();
        let ended = value["ended"].as_object_mut().unwrap();
        ended.remove("address");
        let mut bytes = serde_json::to_vec_pretty(ended).unwrap();
        bytes.push(b'\n');
        assert_eq!(bytes, result.stdout, "{state}");
        // Fixed expectations, independent of the shared implementation: the public
        // `result --wait` contract for each ending state, pinned by literal values.
        let (success, request_state, error) = match state {
            "completed" | "published" => (true, "completed", Value::Null),
            "failed" => (false, "failed", json!("provider failed")),
            "unresolved" | "closed" => (
                false,
                "unresolved",
                json!(
                    "request ended without a published successful result; inspect the session before sending another prompt"
                ),
            ),
            "dead" => (
                false,
                "unresolved",
                json!(
                    "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request"
                ),
            ),
            "recovery_required" => (
                false,
                "recovery_required",
                json!(
                    "completion publication requires recovery; run sessions for this workspace, then query again"
                ),
            ),
            _ => unreachable!(),
        };
        assert_eq!(waited.status.success(), success, "{state}");
        assert_eq!(result.status.success(), success, "{state}");
        assert_eq!(ended["ok"], json!(success), "{state}");
        assert_eq!(top_ok, json!(success), "{state}");
        assert_eq!(ended["request_state"], request_state, "{state}");
        assert_eq!(ended["error"], error, "{state}");
        assert_eq!(top_timed_out, false, "{state}");
        if state == "dead" {
            assert_eq!(ended["owner_process_alive"], false);
        }
        if matches!(state, "completed" | "published") {
            assert_eq!(ended["result"], "result");
            assert!(!ended.contains_key("owner"));
        }
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn hold_and_release_are_idempotent_and_preserve_status() {
    let fixture = Fixture::new();
    let status = fs::read(fixture.directory.join("status.json")).unwrap();
    for (release, expected_changed) in [
        (true, false),
        (false, true),
        (false, false),
        (true, true),
        (true, false),
    ] {
        let before = fs::read(fixture.directory.join("hold.json")).ok();
        let args = if release {
            vec!["hold", "session-observe", "--release", "--json"]
        } else {
            vec!["hold", "session-observe", "--json"]
        };
        let value = success(fixture.run(&args));
        assert_eq!(
            value,
            json!({"schema_version":1, "ok":true, "session":"session-observe",
            "held": !release, "changed":expected_changed})
        );
        let after = fs::read(fixture.directory.join("hold.json")).ok();
        if !expected_changed {
            assert_eq!(before, after);
        }
        if let Some(bytes) = after {
            let record: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(record["held"], true);
            assert!(record["created_unix_ms"].as_u64().unwrap() > 0);
        }
        assert_eq!(
            fs::read(fixture.directory.join("status.json")).unwrap(),
            status
        );
    }
}

#[test]
fn hold_normalizes_malformed_records_and_reports_previous_intent() {
    for release in [false, true] {
        let fixture = Fixture::new();
        fs::write(fixture.directory.join("hold.json"), b"malformed").unwrap();
        let args = if release {
            vec!["hold", "session-observe", "--release", "--json"]
        } else {
            vec!["hold", "session-observe", "--json"]
        };
        let value = success(fixture.run(&args));
        assert_eq!(value["held"], !release);
        assert_eq!(value["changed"], true);
        assert_eq!(value["previous"], "malformed");
    }
}

#[test]
fn hold_refuses_closed_set_and_release_before_reading_hold() {
    let fixture = Fixture::new();
    write(
        &fixture.directory.join("closed.json"),
        &json!({"state":"closed", "generation":3,
        "updated_unix_ms":3, "exit_code":null, "error":null}),
    );
    // A malformed hold must not be normalized after the closed tombstone.
    fs::write(fixture.directory.join("hold.json"), b"malformed").unwrap();
    for release in [false, true] {
        let args = if release {
            vec!["hold", "session-observe", "--release", "--json"]
        } else {
            vec!["hold", "session-observe", "--json"]
        };
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["error"],
            "the session is closed; a hold cannot be set or released"
        );
        assert_eq!(
            fs::read(fixture.directory.join("hold.json")).unwrap(),
            b"malformed"
        );
    }
}

#[test]
fn held_or_unreadable_tell_refuses_before_any_record_write() {
    for malformed in [false, true] {
        let fixture = Fixture::new();
        if malformed {
            fs::write(fixture.directory.join("hold.json"), b"malformed").unwrap();
        } else {
            write(
                &fixture.directory.join("hold.json"),
                &json!({"held":true,"created_unix_ms":1}),
            );
        }
        // No lifecycle lock yet: converge would create it, so compare the whole directory.
        let before = files(fixture.root.path());
        let output = fixture.run(&[
            "tell",
            "session-observe",
            "--prompt",
            "must not send",
            "--json",
        ]);
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let error = if malformed {
            "session session-observe has an unreadable hold record; inspect it before sending a follow-up"
        } else {
            "session session-observe is held; release the hold before sending a follow-up"
        };
        assert_eq!(
            value,
            json!({"schema_version":1,"ok":false,"session":"session-observe","error":error})
        );
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn hold_observations_distinguish_absent_valid_malformed_and_nonregular() {
    for record in ["absent", "held", "malformed", "nonregular"] {
        let fixture = Fixture::new();
        match record {
            "held" => {
                success(fixture.run(&["hold", "session-observe", "--json"]));
            }
            "malformed" => {
                fs::write(fixture.directory.join("hold.json"), b"{\"held\":false}").unwrap();
            }
            "nonregular" => {
                fs::create_dir(fixture.directory.join("hold.json")).unwrap();
            }
            _ => (),
        }
        fixture.event("event-1.json", "retained result");
        let before = files(fixture.root.path());
        let inspect = success(fixture.run(&["inspect", "session-observe", "--json"]));
        let status = success(fixture.run(&["status", "--all-workspaces", "--json"]));
        let entry = &status["sessions"][0];
        let held = match record {
            "held" => json!(true),
            "absent" => json!(false),
            _ => Value::Null,
        };
        assert_eq!(inspect["held"], held);
        assert_eq!(entry["held"], held);
        assert_eq!(inspect.get("hold_error").is_some(), held.is_null());
        assert_eq!(entry.get("hold_error").is_some(), held.is_null());
        let flags = entry["attention"].as_array().unwrap();
        assert_eq!(flags.contains(&json!("held")), held == true);
        assert_eq!(
            flags.contains(&json!("records_partially_unreadable")),
            held.is_null()
        );
        let result = success(fixture.run(&[
            "result",
            "session-observe",
            "--event",
            "event-1.json",
            "--json",
        ]));
        assert_eq!(result["result"], "retained result");
        let timeline =
            success(fixture.run(&["inspect", "session-observe", "--timeline", "--json"]));
        assert!(timeline.get("held").is_none());
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn hold_argument_and_missing_session_errors_are_json() {
    let fixture = Fixture::new();
    for args in [
        vec!["hold", "--json"],
        vec!["hold", "session-missing", "--json"],
        vec!["hold", "session-observe", "--unknown", "--json"],
        vec![
            "hold",
            "session-observe",
            "--release",
            "--release",
            "--json",
        ],
        vec!["hold", "session-observe", "session-other", "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["ok"], false);
        assert!(value["error"].is_string());
    }
    fs::write(fixture.directory.join("manifest.json"), b"malformed").unwrap();
    let before = files(fixture.root.path());
    assert!(
        !fixture
            .run(&["hold", "session-observe", "--json"])
            .status
            .success()
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn close_preserves_hold_and_prune_removes_its_directory() {
    let fixture = Fixture::new();
    success(fixture.run(&["hold", "session-observe", "--json"]));
    let held = fs::read(fixture.directory.join("hold.json")).unwrap();
    let close = fixture.run(&["close-session", "session-observe", "--explicit"]);
    assert!(
        close.status.success(),
        "{}",
        String::from_utf8_lossy(&close.stderr)
    );
    assert_eq!(fs::read(fixture.directory.join("hold.json")).unwrap(), held);
    let status = success(fixture.run(&["status", "--all-workspaces", "--all", "--json"]));
    assert_eq!(status["sessions"][0]["held"], true);
    assert_eq!(status["sessions"][0]["state"], "closed");
    // Age the retained records without waiting for the public one-day minimum.
    for name in ["closed.json", "status.json"] {
        let path = fixture.directory.join(name);
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record["updated_unix_ms"] = json!(1);
        write(&path, &record);
    }
    let pruned = success(fixture.run(&[
        "prune-sessions",
        "--closed-before-days",
        "1",
        "--explicit",
        "--json",
    ]));
    assert_eq!(pruned["pruned"], json!(["session-observe"]));
    assert!(!fixture.directory.exists());
}

fn cancel_fixture() -> Fixture {
    let fixture = Fixture::new();
    fs::create_dir(fixture.directory.join("requests")).unwrap();
    write(
        &fixture.directory.join("requests/1-2-3.json"),
        &json!({
            "schema":1, "request_id":"request-cancel", "claim_token":"1-2-3",
            "event_file":"event-1.json", "created_unix_ms":2
        }),
    );
    write(
        &fixture.directory.join("cancel.json"),
        &json!({
            "schema":1, "request_id":"request-cancel", "claim_token":"1-2-3", "created_unix_ms":3
        }),
    );
    fixture
}

#[test]
fn cancel_readers_render_outcomes_without_mutation() {
    for cancelled in [false, true] {
        let fixture = cancel_fixture();
        write(
            &fixture.directory.join("events/event-1.json"),
            &json!({
                "provider":"claude", "message":if cancelled { "" } else { "normal completion" },
                "error":if cancelled { Some("cancelled: aborted") } else { None }, "cancelled":cancelled,
                "provider_session_id":"native", "turn_id":"turn", "created_unix_ms":4
            }),
        );
        let before = files(fixture.root.path());
        let inspect = success(fixture.run(&["inspect", "session-observe", "--json"]));
        assert_eq!(
            inspect["cancel"],
            json!({"request_id":"request-cancel", "requested_unix_ms":3, "state": if cancelled { "applied" } else { "not_applied" }})
        );
        let result = success(fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-cancel",
            "--json",
        ]));
        assert_eq!(
            result["request_state"],
            if cancelled { "cancelled" } else { "completed" }
        );
        let waited = fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-cancel",
            "--wait",
            "--json",
        ]);
        assert_eq!(waited.status.success(), !cancelled);
        let value: Value = serde_json::from_slice(&waited.stdout).unwrap();
        assert_eq!(value["ok"], !cancelled);
        assert_eq!(value["request_state"], result["request_state"]);
        let waited = fixture.run(&["wait", "session-observe/request-cancel", "--json"]);
        assert_eq!(waited.status.success(), !cancelled);
        let timeline =
            success(fixture.run(&["inspect", "session-observe", "--timeline", "--json"]));
        let entry = timeline["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["stage"] == "cancel_request")
            .unwrap();
        assert_eq!(entry["request_id"], "request-cancel");
        assert_eq!(
            entry["cancel"],
            json!({"state":if cancelled { "applied" } else { "not_applied" }, "derived_from":"result"})
        );
        let doctor: Value =
            serde_json::from_slice(&fixture.run(&["doctor", "session-observe", "--json"]).stdout)
                .unwrap();
        assert!(
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == "cancel" && c["reason_code"] == "cancel_recorded")
        );
        let search = success(fixture.run(&["search", "aborted", "--all-workspaces", "--json"]));
        assert!(search["hits"].as_array().unwrap().is_empty());
        assert_eq!(files(fixture.root.path()), before);
    }
}

#[test]
fn cancel_requested_unreadable_and_absent_are_auxiliary_evidence() {
    let fixture = cancel_fixture();
    fs::write(fixture.directory.join("turn.claim"), "1-2-3\n").unwrap();
    write(
        &fixture.directory.join("status.json"),
        &json!({"state":"working", "generation":3, "updated_unix_ms":3, "error":null, "exit_code":null}),
    );
    let before = files(fixture.root.path());
    let inspected = success(fixture.run(&["inspect", "session-observe", "--json"]));
    assert_eq!(inspected["cancel"]["state"], "requested");
    let status = success(fixture.run(&["status", "--all-workspaces", "--json"]));
    assert!(status.to_string().contains("cancel_requested"));
    assert_eq!(files(fixture.root.path()), before);
    fs::remove_file(fixture.directory.join("turn.claim")).unwrap();
    fixture.event("event-1.json", "published success");
    fs::write(fixture.directory.join("cancel.json"), b"{broken").unwrap();
    let before = files(fixture.root.path());
    let inspected = success(fixture.run(&["inspect", "session-observe", "--json"]));
    assert_eq!(inspected["cancel"]["state"], "unreadable");
    assert_eq!(inspected["latest_result"]["request_state"], "completed");
    let timeline = success(fixture.run(&["inspect", "session-observe", "--timeline", "--json"]));
    assert_eq!(timeline["incomplete"], true);
    assert!(
        timeline["session_entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["stage"] == "cancel_request" && e["record_state"] == "unreadable")
    );
    let doctor: Value =
        serde_json::from_slice(&fixture.run(&["doctor", "session-observe", "--json"]).stdout)
            .unwrap();
    assert!(
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "cancel" && c["reason_code"] == "cancel_unreadable")
    );
    assert_eq!(files(fixture.root.path()), before);
    fs::remove_file(fixture.directory.join("cancel.json")).unwrap();
    assert_eq!(
        success(fixture.run(&["inspect", "session-observe", "--json"]))["cancel"],
        Value::Null
    );
    let doctor: Value =
        serde_json::from_slice(&fixture.run(&["doctor", "session-observe", "--json"]).stdout)
            .unwrap();
    assert!(
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "cancel" && c["reason_code"] == "cancel_absent")
    );
}

#[test]
fn cancel_command_refusal_envelope_and_parse_errors() {
    let fixture = cancel_fixture();
    let output = fixture.run(&["cancel", "session-observe", "--json"]);
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({"schema_version":1,"ok":false,"session":"session-observe","error":"session session-observe is ready; cancel requires the running or working state"})
    );
    for args in [
        vec!["cancel", "--json"],
        vec!["cancel", "session-observe", "--json", "--release"],
        vec!["cancel", "session-observe", "--json", "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], false);
    }
}

#[test]
fn cancel_all_providers_refuse_without_changing_claim_or_intent() {
    for provider in ["codex", "claude", "agy", "pi"] {
        for state in ["running", "working"] {
            let fixture = cancel_fixture();
            fs::remove_file(fixture.directory.join("cancel.json")).unwrap();
            let path = fixture.directory.join("manifest.json");
            let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            manifest["provider"] = json!(provider);
            write(&path, &manifest);
            write(
                &fixture.directory.join("status.json"),
                &json!({"state":state,"generation":3,"updated_unix_ms":3,"error":null,"exit_code":null}),
            );
            fs::write(fixture.directory.join("turn.claim"), "1-2-3\n").unwrap();
            // The command may create its lifecycle lock; all other bytes must be retained.
            let before = files(fixture.root.path());
            let output = fixture.run(&["cancel", "session-observe", "--json"]);
            assert!(!output.status.success());
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                value,
                json!({"schema_version":1,"ok":false,"session":"session-observe",
                "error":format!("in this release the Bridge integration for {provider} does not support cancel")})
            );
            let mut after = files(fixture.root.path());
            after.retain(|(path, _)| path.file_name().unwrap() != "turn.claim.lock");
            assert_eq!(before, after);
        }
    }
}

#[test]
fn cancel_timeline_filters_retained_intent_and_rejects_non_utf8() {
    let fixture = cancel_fixture();
    write(
        &fixture.directory.join("requests/4-5-6.json"),
        &json!({
            "schema":1,"request_id":"request-other","claim_token":"4-5-6","event_file":"event-2.json","created_unix_ms":4
        }),
    );
    let value = success(fixture.run(&[
        "inspect",
        "session-observe",
        "--timeline",
        "--request",
        "request-other",
        "--json",
    ]));
    assert!(
        !value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["stage"] == "cancel_request")
    );
    fs::write(fixture.directory.join("cancel.json"), [0xff]).unwrap();
    let before = files(fixture.root.path());
    let value = success(fixture.run(&["inspect", "session-observe", "--timeline", "--json"]));
    assert_eq!(value["incomplete"], true);
    assert!(
        value["session_entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["stage"] == "cancel_request" && e["record_state"] == "unreadable")
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn search_excludes_cancelled_even_when_stored_event_has_a_body() {
    let fixture = cancel_fixture();
    let mut event = fixture.event("event-1.json", "needle");
    event["cancelled"] = json!(true);
    write(&fixture.directory.join("events/event-1.json"), &event);
    let found = success(fixture.run(&["search", "needle", "--all-workspaces", "--json"]));
    assert!(found["hits"].as_array().unwrap().is_empty());
}

#[test]
fn cancel_identity_mismatch_is_unreadable_in_every_reader_without_hiding_result() {
    for cancelled in [false, true] {
        let fixture = cancel_fixture();
        let mut event = fixture.event("event-1.json", "published result");
        if cancelled {
            event["message"] = json!("");
            event["error"] = json!("cancelled: interrupted");
            event["cancelled"] = json!(true);
            write(&fixture.directory.join("events/event-1.json"), &event);
        }
        write(
            &fixture.directory.join("cancel.json"),
            &json!({
                "schema":1, "request_id":"request-cancel", "claim_token":"4-5-6", "created_unix_ms":3
            }),
        );
        let before = files(fixture.root.path());
        let inspect = success(fixture.run(&["inspect", "session-observe", "--json"]));
        assert_eq!(inspect["cancel"]["state"], "unreadable");
        let result = success(fixture.run(&[
            "result",
            "session-observe",
            "--request",
            "request-cancel",
            "--json",
        ]));
        assert_eq!(
            result["request_state"],
            if cancelled { "cancelled" } else { "completed" }
        );
        assert_eq!(
            inspect["latest_result"]["request_state"],
            result["request_state"]
        );
        let timeline =
            success(fixture.run(&["inspect", "session-observe", "--timeline", "--json"]));
        assert_eq!(timeline["incomplete"], true);
        let entry = timeline["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["stage"] == "cancel_request")
            .unwrap();
        assert_eq!(entry["cancel"]["state"], inspect["cancel"]["state"]);
        assert_eq!(entry["cancel"]["derived_from"], "result");
        let doctor: Value =
            serde_json::from_slice(&fixture.run(&["doctor", "session-observe", "--json"]).stdout)
                .unwrap();
        assert!(
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == "cancel" && c["reason_code"] == "cancel_unreadable")
        );
        let status = success(fixture.run(&["status", "--all-workspaces", "--json"]));
        assert!(!status.to_string().contains("cancel_requested"));
        assert_eq!(files(fixture.root.path()), before);
    }
}
