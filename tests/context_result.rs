//! `--context-result` resolves attached results read-only, before any claim, receipt,
//! launch, or delivery. Live provider flows (Codex -> Claude -> Codex) are not exercised
//! here; these fixtures are hand-written session records under a private state root.
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SOURCE: &str = "session-source";
const TARGET: &str = "session-target";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        fixture.session(SOURCE, "codex", "closed");
        fixture.session(TARGET, "claude", "ready");
        fixture
    }

    fn directory(&self, id: &str) -> PathBuf {
        self.root.path().join(id)
    }

    fn session(&self, id: &str, provider: &str, state: &str) {
        let directory = self.directory(id);
        fs::create_dir_all(directory.join("events")).unwrap();
        write(
            &directory.join("manifest.json"),
            &json!({
                "schema": 1, "id": id, "provider": provider, "provider_path": provider,
                "provider_version": "0.0.0", "workspace": self.root.path().canonicalize().unwrap(),
                "title": "context fixture", "model": null, "effort": null, "yolo": false,
                "created_unix_ms": 1
            }),
        );
        write(
            &directory.join("status.json"),
            &json!({"state": state, "generation": 2, "updated_unix_ms": 2, "exit_code": null, "error": null}),
        );
    }

    fn event(&self, id: &str, name: &str, message: &str, error: Option<&str>) {
        write(
            &self.directory(id).join("events").join(name),
            &json!({"provider": "codex", "message": message, "error": error,
                "provider_session_id": "thread", "turn_id": name, "created_unix_ms": 3}),
        );
    }

    fn receipt(&self, id: &str, claim: &str, request: &str, event: &str, sources: Value) {
        let directory = self.directory(id).join("requests");
        fs::create_dir_all(&directory).unwrap();
        let mut receipt = json!({"schema": 1, "request_id": request, "claim_token": claim,
            "event_file": event, "created_unix_ms": 3});
        if !sources.is_null() {
            receipt["context_sources"] = sources;
        }
        write(&directory.join(format!("{claim}.json")), &receipt);
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn files(&self) -> Vec<Entry> {
        files(self.root.path())
    }

    fn ask_with(&self, address: &str) -> Output {
        self.run(&[
            "ask",
            "claude",
            "--workspace",
            self.root.path().to_str().unwrap(),
            "--prompt",
            "start from the attached result",
            "--context-result",
            address,
            "--json",
        ])
    }
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

/// Every path under the state root: directories carry no content, files carry their bytes,
/// so an added directory is as visible as an added file.
type Entry = (PathBuf, Option<Vec<u8>>);

fn files(directory: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            entries.push((entry.path(), None));
            entries.extend(files(&entry.path()));
        } else {
            entries.push((entry.path(), Some(fs::read(entry.path()).unwrap())));
        }
    }
    entries.sort();
    entries
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tell_with(fixture: &Fixture, address: &str) -> Output {
    fixture.run(&[
        "tell",
        TARGET,
        "--prompt",
        "continue from the attached result",
        "--context-result",
        address,
        "--json",
    ])
}

fn assert_unattachable(fixture: &Fixture, output: Output, address: &str, state: &str) {
    assert!(!output.status.success(), "{address} unexpectedly succeeded");
    let text = stderr(&output);
    assert!(text.contains(address), "{address}: {text}");
    assert!(
        text.contains(&format!("request_state is {state}")),
        "{address}: {text}"
    );
    let (session, id) = address.split_once('/').unwrap();
    let flag = if id.starts_with("request-") {
        "--request"
    } else {
        "--event"
    };
    assert!(
        text.contains(&format!("tabcli result {session} {flag} {id} --json")),
        "{address}: {text}"
    );
    // Rejection precedes any receipt, so there is no request address to report.
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fixture.directory(TARGET).join("turn.claim").exists());
    assert!(!fixture.directory(TARGET).join("turn.claim.lock").exists());
    assert!(!fixture.directory(TARGET).join("requests").exists());
    assert!(
        !fixture
            .directory(TARGET)
            .join("turn.completion.json")
            .exists()
    );
    assert!(
        fixture
            .directory(TARGET)
            .join("events")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn every_unpublished_or_unsuccessful_source_fails_before_the_target_is_claimed() {
    let fixture = Fixture::new();
    fixture.receipt(
        SOURCE,
        "1-1-1",
        "request-pending",
        "event-1.json",
        Value::Null,
    );
    fs::write(fixture.directory(SOURCE).join("turn.claim"), "1-1-1\n").unwrap();
    fixture.receipt(
        SOURCE,
        "1-1-2",
        "request-failed",
        "event-2.json",
        Value::Null,
    );
    fixture.event(
        SOURCE,
        "event-2.json",
        "failure details",
        Some("provider failed"),
    );
    fixture.receipt(
        SOURCE,
        "1-1-3",
        "request-unresolved",
        "event-3.json",
        Value::Null,
    );
    fixture.receipt(
        SOURCE,
        "1-1-4",
        "request-corrupt",
        "event-4.json",
        Value::Null,
    );
    fs::write(
        fixture.directory(SOURCE).join("events/event-4.json"),
        "not JSON",
    )
    .unwrap();
    fixture.event(SOURCE, "event-5.json", "committed later", None);
    write(
        &fixture.directory(SOURCE).join("turn.completion.json"),
        &json!({"schema": 1, "claim_token": "1-1-1", "event_file": "event-5.json",
            "event": {"provider": "codex", "message": "committed later", "error": null,
                "provider_session_id": "thread", "turn_id": "t", "created_unix_ms": 3},
            "status_error": null, "status_state": "ready"}),
    );
    let before = fixture.files();
    let status_before = fs::read(fixture.directory(TARGET).join("status.json")).unwrap();
    for (address, state) in [
        (format!("{SOURCE}/request-pending"), "pending"),
        (format!("{SOURCE}/request-failed"), "failed"),
        (format!("{SOURCE}/request-unresolved"), "unresolved"),
        (format!("{SOURCE}/request-corrupt"), "unreadable"),
        (format!("{SOURCE}/request-unknown"), "unreadable"),
        (format!("{SOURCE}/event-5.json"), "recovery_required"),
        (format!("{SOURCE}/event-404.json"), "unreadable"),
        ("session-pruned/request-1".to_owned(), "missing"),
    ] {
        let output = tell_with(&fixture, &address);
        assert_unattachable(&fixture, output, &address, state);
        assert_eq!(fixture.files(), before, "{address} changed the state root");
        assert_eq!(
            fs::read(fixture.directory(TARGET).join("status.json")).unwrap(),
            status_before
        );
    }
    let output = fixture.ask_with(&format!("{SOURCE}/request-pending"));
    assert!(!output.status.success());
    assert!(stderr(&output).contains(&format!("{SOURCE}/request-pending")));
    assert_eq!(
        fixture.files(),
        before,
        "ask created or changed session state"
    );
}

#[test]
fn busy_source_snapshots_fail_resolution_instead_of_guessing() {
    let fixture = Fixture::new();
    fixture.receipt(SOURCE, "1-1-1", "request-done", "event-1.json", Value::Null);
    fixture.event(SOURCE, "event-1.json", "recorded answer", None);
    let lock = fs::File::create(fixture.directory(SOURCE).join("turn.claim.lock")).unwrap();
    // Snapshot the tree before locking: Windows refuses to read a file another process
    // has locked, and the comparison after release covers the same set of files.
    let before = fixture.files();
    lock.lock().unwrap();
    let address = format!("{SOURCE}/request-done");
    let output = tell_with(&fixture, &address);
    drop(lock);
    assert_unattachable(&fixture, output, &address, "busy");
    assert_eq!(fixture.files(), before);
}

#[test]
fn published_successful_results_pass_resolution_and_legacy_events_have_no_request() {
    let fixture = Fixture::new();
    fixture.receipt(SOURCE, "1-1-1", "request-done", "event-1.json", Value::Null);
    fixture.event(SOURCE, "event-1.json", "recorded answer", None);
    fixture.event(SOURCE, "event-0.json", "legacy answer", None);
    let before = fixture.files();
    for address in [
        format!("{SOURCE}/request-done"),
        format!("{SOURCE}/event-0.json"),
    ] {
        // The fixture target has no terminal handle, so the command still fails, but only
        // after resolution: the failure is about the target, not about the attachment.
        let output = tell_with(&fixture, &address);
        assert!(!output.status.success());
        let text = stderr(&output);
        assert!(!text.contains("context result"), "{address}: {text}");
        assert!(!text.contains("cannot be attached"), "{address}: {text}");
        assert!(text.contains("terminal.json"), "{address}: {text}");
        // Passing resolution lets the existing tell path open the target's empty lifecycle
        // lock file, as any tell does before it reads the terminal handle. Nothing else
        // may change, and no claim, receipt, or event exists.
        let lock = fixture.directory(TARGET).join("turn.claim.lock");
        let after = fixture
            .files()
            .into_iter()
            .filter(|(path, content)| {
                if path == &lock {
                    assert_eq!(
                        content.as_deref(),
                        Some(&[][..]),
                        "{address}: lock has content"
                    );
                    return false;
                }
                true
            })
            .collect::<Vec<_>>();
        assert_eq!(after, before, "{address} changed the state root");
        assert!(!fixture.directory(TARGET).join("turn.claim").exists());
        assert!(!fixture.directory(TARGET).join("requests").exists());
    }
    // A legacy event is addressable only by its event id, which `result` reports without a
    // request mapping; that is exactly what the provenance will record.
    let result = success(fixture.run(&["result", SOURCE, "--event", "event-0.json", "--json"]));
    assert_eq!(result["request_state"], "completed");
    assert_eq!(result["request_id"], Value::Null);
    assert_eq!(result["context_sources"], json!([]));
}

#[test]
fn inspect_and_result_expose_recorded_context_sources_read_only() {
    let fixture = Fixture::new();
    let sources = json!([
        {"session": SOURCE, "request_id": "request-parent", "event_id": "event-1.json",
            "provider": "codex", "created_unix_ms": 3},
        {"session": SOURCE, "request_id": null, "event_id": "event-0.json",
            "provider": "codex", "created_unix_ms": 3}
    ]);
    fixture.receipt(
        TARGET,
        "2-2-1",
        "request-derived",
        "event-10.json",
        sources.clone(),
    );
    fixture.receipt(
        TARGET,
        "2-2-2",
        "request-plain",
        "event-11.json",
        Value::Null,
    );
    fixture.event(TARGET, "event-10.json", "derived answer", None);
    fixture.event(TARGET, "event-11.json", "plain answer", None);
    let before = fixture.files();

    let inspected = success(fixture.run(&["inspect", TARGET, "--json"]));
    let requests = inspected["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["request_id"], "request-derived");
    assert_eq!(requests[0]["context_sources"], sources);
    assert_eq!(requests[1]["request_id"], "request-plain");
    assert_eq!(requests[1]["context_sources"], json!([]));

    let listed = success(fixture.run(&["result", TARGET, "--list", "--json"]));
    let listed = listed["requests"].as_array().unwrap();
    assert_eq!(listed[0]["context_sources"], sources);
    assert_eq!(listed[1]["context_sources"], json!([]));

    let single =
        success(fixture.run(&["result", TARGET, "--request", "request-derived", "--json"]));
    assert_eq!(single["result"], "derived answer");
    assert_eq!(single["context_sources"], sources);
    let plain = success(fixture.run(&["result", TARGET, "--request", "request-plain", "--json"]));
    assert_eq!(plain["context_sources"], json!([]));
    let by_event = success(fixture.run(&["result", TARGET, "--event", "event-10.json", "--json"]));
    assert_eq!(by_event["context_sources"], sources);
    assert_eq!(fixture.files(), before);
}

#[test]
fn context_result_arguments_are_validated_before_any_session_is_read() {
    let fixture = Fixture::new();
    let before = fixture.files();
    for (arguments, expected) in [
        (
            vec![
                "tell",
                TARGET,
                "--prompt",
                "x",
                "--context-result",
                "session-source/latest",
            ],
            "invalid --context-result",
        ),
        (
            vec![
                "tell",
                TARGET,
                "--prompt",
                "x",
                "--context-result",
                "request-done",
            ],
            "invalid --context-result",
        ),
        (
            vec![
                "tell",
                TARGET,
                "--prompt",
                "x",
                "--context-result",
                "session-source/request-done",
                "--context-result",
                "session-source/request-done",
            ],
            "more than once",
        ),
        (
            vec![
                "tell",
                TARGET,
                "--context-result",
                "session-source/request-done",
            ],
            "requires --prompt",
        ),
    ] {
        let output = fixture.run(&arguments);
        assert!(!output.status.success());
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
    }
    assert_eq!(fixture.files(), before);
}

#[test]
fn a_source_the_receipt_would_reject_fails_resolution_before_ask_creates_anything() {
    let fixture = Fixture::new();
    let manifest = fixture.directory(SOURCE).join("manifest.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    value["provider"] = json!("");
    write(&manifest, &value);
    fixture.receipt(SOURCE, "1-1-1", "request-done", "event-1.json", Value::Null);
    fixture.event(SOURCE, "event-1.json", "recorded answer", None);
    let before = fixture.files();
    let address = format!("{SOURCE}/request-done");
    let output = fixture.ask_with(&address);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(
        text.contains(&address)
            && text.contains("request_state is unreadable")
            && text.contains("invalid recorded provenance")
            && !text.contains("invalid Bridge request receipt"),
        "{text}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        fixture.files(),
        before,
        "ask created or changed session state"
    );
    let output = tell_with(&fixture, &address);
    assert_unattachable(&fixture, output, &address, "unreadable");
    assert_eq!(fixture.files(), before);
}

#[test]
fn unmapped_events_are_unverifiable_while_the_source_request_index_is_damaged() {
    let fixture = Fixture::new();
    fixture.receipt(SOURCE, "1-1-1", "request-done", "event-1.json", Value::Null);
    fixture.event(SOURCE, "event-1.json", "recorded answer", None);
    fixture.event(SOURCE, "event-0.json", "legacy answer", None);
    fs::write(
        fixture.directory(SOURCE).join("requests/1-1-9.json"),
        "invalid",
    )
    .unwrap();
    let before = fixture.files();
    let address = format!("{SOURCE}/event-0.json");
    let output = tell_with(&fixture, &address);
    assert_unattachable(&fixture, output, &address, "unverifiable");
    assert_eq!(fixture.files(), before);
    // `result` itself still reports the event as completed; only attachment refuses it.
    let result = success(fixture.run(&["result", SOURCE, "--event", "event-0.json", "--json"]));
    assert_eq!(result["request_state"], "completed");
    assert_eq!(result["unreadable_requests"], 1);
    // The event with a readable receipt passes resolution through both addresses.
    for address in [
        format!("{SOURCE}/request-done"),
        format!("{SOURCE}/event-1.json"),
    ] {
        let output = tell_with(&fixture, &address);
        let text = stderr(&output);
        assert!(!text.contains("cannot be attached"), "{address}: {text}");
        assert!(text.contains("terminal.json"), "{address}: {text}");
    }
}

#[test]
fn invalid_utf8_in_a_recorded_event_is_never_repaired_or_attached() {
    let fixture = Fixture::new();
    fixture.receipt(SOURCE, "1-1-1", "request-done", "event-1.json", Value::Null);
    fs::write(
        fixture.directory(SOURCE).join("events/event-1.json"),
        b"{\"provider\":\"codex\",\"message\":\"bad\xfftext\",\"error\":null,\
          \"provider_session_id\":\"thread\",\"turn_id\":\"t\",\"created_unix_ms\":3}",
    )
    .unwrap();
    let before = fixture.files();
    let result = success(fixture.run(&["result", SOURCE, "--request", "request-done", "--json"]));
    assert_eq!(result["request_state"], "completed");
    for address in [
        format!("{SOURCE}/request-done"),
        format!("{SOURCE}/event-1.json"),
    ] {
        let output = tell_with(&fixture, &address);
        let text = stderr(&output);
        assert!(text.contains("not valid UTF-8"), "{address}: {text}");
        assert!(!text.contains('\u{fffd}'), "{address}: {text}");
        assert_unattachable(&fixture, output, &address, "unreadable");
        assert_eq!(fixture.files(), before);
    }
}

#[test]
fn case_aliases_of_a_recorded_event_name_never_pass_resolution() {
    let fixture = Fixture::new();
    fixture.event(SOURCE, "event-a.json", "answer", None);
    fixture.receipt(
        SOURCE,
        "1-1-1",
        "request-alias",
        "event-A.json",
        Value::Null,
    );
    let before = fixture.files();
    // Windows and macOS APFS resolve the alias to event-a.json, so the exact-name check
    // must refuse it; on a case-sensitive filesystem the aliased file is simply absent.
    let case_insensitive = fixture
        .directory(SOURCE)
        .join("events")
        .join("event-A.json")
        .exists();
    let address = format!("{SOURCE}/event-A.json");
    let output = tell_with(&fixture, &address);
    if case_insensitive {
        assert!(
            stderr(&output).contains("does not match an events/ entry exactly"),
            "{}",
            stderr(&output)
        );
    }
    assert_unattachable(&fixture, output, &address, "unreadable");
    assert_eq!(fixture.files(), before);
    let address = format!("{SOURCE}/request-alias");
    let output = tell_with(&fixture, &address);
    let state = if case_insensitive {
        "unreadable"
    } else {
        "unresolved"
    };
    assert_unattachable(&fixture, output, &address, state);
    assert_eq!(fixture.files(), before);
}
