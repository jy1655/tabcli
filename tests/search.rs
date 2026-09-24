use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A private state root with a workspace directory per session. Sessions are created
/// by hand so the search corpus, receipts, claims, and corruption are all explicit.
struct Fixture {
    root: tempfile::TempDir,
    workspace_a: PathBuf,
    workspace_b: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace_a = root.path().join("workspace-a");
        let workspace_b = root.path().join("workspace-b");
        fs::create_dir(&workspace_a).unwrap();
        fs::create_dir(&workspace_b).unwrap();
        Self {
            root,
            workspace_a: workspace_a.canonicalize().unwrap(),
            workspace_b: workspace_b.canonicalize().unwrap(),
        }
    }

    fn session(&self, id: &str, provider: &str, workspace: &Path) -> PathBuf {
        let directory = self.root.path().join(id);
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        write(
            &directory.join("manifest.json"),
            &json!({
                "schema": 1, "id": id, "provider": provider, "provider_path": provider,
                "provider_version": "1.0.0", "workspace": workspace, "title": format!("{id} title"),
                "model": null, "effort": null, "yolo": false, "created_unix_ms": 1
            }),
        );
        write(
            &directory.join("status.json"),
            &json!({
                "state": "ready", "generation": 2, "updated_unix_ms": 2,
                "exit_code": null, "error": null
            }),
        );
        directory
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .current_dir(cwd)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            // Opens the undocumented `--max-bytes` test aid, which the release binary
            // otherwise rejects as an unknown option.
            .env("AGENT_BRIDGE_TEST_SEARCH_AIDS", "1")
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.workspace_a, args)
    }

    /// The binary exactly as a user runs it: the test aids are closed.
    fn run_as_user(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .current_dir(&self.workspace_a)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .env_remove("AGENT_BRIDGE_TEST_SEARCH_AIDS")
            .output()
            .unwrap()
    }

    fn search(&self, args: &[&str]) -> Value {
        let mut full = vec!["search"];
        full.extend_from_slice(args);
        full.push("--json");
        success(self.run(&full))
    }
}

fn event(directory: &Path, name: &str, text: &str, created_unix_ms: u64) -> Value {
    let event = json!({ "provider": "claude", "message": text, "error": null,
        "provider_session_id": "native-session", "turn_id": name, "created_unix_ms": created_unix_ms });
    write(&directory.join("events").join(name), &event);
    event
}

fn receipt(directory: &Path, claim: &str, id: &str, event: &str) {
    let requests = directory.join("requests");
    fs::create_dir_all(&requests).unwrap();
    write(
        &requests.join(format!("{claim}.json")),
        &json!({
            "schema": 1, "request_id": id, "claim_token": claim,
            "event_file": event, "created_unix_ms": 3
        }),
    );
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

fn sessions(value: &Value) -> Vec<&str> {
    value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["session"].as_str().unwrap())
        .collect()
}

#[test]
fn default_scope_is_the_current_workspace_and_flags_widen_it() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    let b = fixture.session("session-b", "codex", &fixture.workspace_b);
    event(&a, "event-1.json", "native queue review from a", 10);
    event(&b, "event-1.json", "native queue review from b", 20);

    let current = fixture.search(&["native queue"]);
    assert_eq!(current["ok"], true);
    assert_eq!(current["schema_version"], 1);
    assert_eq!(sessions(&current), ["session-a"]);
    assert_eq!(current["filters"]["workspace"], json!(fixture.workspace_a));
    assert_eq!(current["filters"]["all_workspaces"], false);
    assert_eq!(current["filters"]["provider"], Value::Null);
    assert_eq!(current["limit"], 20);
    assert_eq!(current["truncated"], false);
    assert_eq!(current["incomplete"], false);
    assert_eq!(current["scanned"]["sessions"], 1);
    assert_eq!(current["scanned"]["events"], 1);
    let hit = &current["hits"][0];
    assert_eq!(hit["provider"], "claude");
    assert_eq!(hit["title"], "session-a title");
    assert_eq!(hit["workspace"], json!(fixture.workspace_a));
    assert_eq!(hit["event_id"], "event-1.json");
    assert_eq!(hit["created_unix_ms"], 10);

    let other = fixture.search(&[
        "native queue",
        "--workspace",
        fixture.workspace_b.to_str().unwrap(),
    ]);
    assert_eq!(sessions(&other), ["session-b"]);
    assert_eq!(other["filters"]["workspace"], json!(fixture.workspace_b));

    // The same relative path that `sessions --workspace` accepts.
    let relative = success(fixture.run_in(
        fixture.root.path(),
        &[
            "search",
            "native queue",
            "--workspace",
            "workspace-b",
            "--json",
        ],
    ));
    assert_eq!(sessions(&relative), ["session-b"]);

    let all = fixture.search(&["native queue", "--all-workspaces"]);
    assert_eq!(sessions(&all), ["session-b", "session-a"]);
    assert_eq!(all["filters"]["workspace"], Value::Null);
    assert_eq!(all["filters"]["all_workspaces"], true);
    assert_eq!(all["scanned"]["sessions"], 2);
}

#[test]
fn provider_filter_and_sort_order_are_stable() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    let b = fixture.session("session-b", "codex", &fixture.workspace_a);
    event(&a, "event-2.json", "shared text", 5);
    event(&a, "event-1.json", "shared text", 5);
    event(&b, "event-1.json", "shared text", 5);
    event(&b, "event-3.json", "shared text", 9);

    let all = fixture.search(&["shared"]);
    let addresses: Vec<(String, String)> = all["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["session"].as_str().unwrap().to_owned(),
                hit["event_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        addresses,
        [
            ("session-b".to_owned(), "event-3.json".to_owned()),
            ("session-a".to_owned(), "event-1.json".to_owned()),
            ("session-a".to_owned(), "event-2.json".to_owned()),
            ("session-b".to_owned(), "event-1.json".to_owned()),
        ]
    );

    let codex = fixture.search(&["shared", "--provider", "codex"]);
    assert_eq!(sessions(&codex), ["session-b", "session-b"]);
    assert_eq!(codex["filters"]["provider"], "codex");
    assert_eq!(codex["scanned"]["sessions"], 1);
}

#[test]
fn identical_bodies_in_two_requests_stay_two_addressable_hits() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    receipt(&a, "123-456-0", "request-first", "event-1.json");
    receipt(&a, "123-457-1", "request-second", "event-2.json");
    event(&a, "event-1.json", "identical result", 3);
    event(&a, "event-2.json", "identical result", 3);

    let found = fixture.search(&["identical"]);
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["request_id"], "request-first");
    assert_eq!(
        hits[0]["result_command"],
        "agent-bridge result session-a --request request-first --json"
    );
    assert_eq!(hits[1]["request_id"], "request-second");
    assert_eq!(hits[1]["event_id"], "event-2.json");
}

#[test]
fn legacy_events_are_searchable_without_inventing_request_identity() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    event(&a, "event-1.json", "legacy review result", 3);

    let found = fixture.search(&["legacy"]);
    let hit = &found["hits"][0];
    assert_eq!(hit["request_id"], Value::Null);
    assert_eq!(hit["event_id"], "event-1.json");
    assert_eq!(
        hit["result_command"],
        "agent-bridge result session-a --event event-1.json --json"
    );
}

#[test]
fn pending_claimed_and_failed_events_are_not_results() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    receipt(&a, "123-456-0", "request-pending", "event-1.json");
    event(&a, "event-1.json", "unpublished pending text", 3);
    fs::write(a.join("turn.claim"), "123-456-0\n").unwrap();
    let mut failed = event(&a, "event-2.json", "failed provider text", 4);
    failed["error"] = json!("provider failed");
    write(&a.join("events/event-2.json"), &failed);
    let journaled = event(&a, "event-3.json", "journaled completion text", 5);
    write(
        &a.join("turn.completion.json"),
        &json!({
            "schema": 1, "claim_token": "123-456-0", "event_file": "event-3.json",
            "event": journaled, "status_error": null, "status_state": "ready"
        }),
    );
    event(&a, "event-4.json", "published text", 6);
    let before = files(fixture.root.path());

    let found = fixture.search(&["text"]);
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["event_id"], "event-4.json");
    // The journaled event was read to decide publication (its key order differs from
    // the journal's write), skipped, and reported; the read counts as a scanned event.
    assert_eq!(found["incomplete"], true, "{found}");
    assert_eq!(found["scanned"]["events"], 3, "{found}");
    assert_eq!(
        found["incomplete_reasons"],
        json!([{"session": "session-a", "reason":
            "event-3.json: skipped; the event differs from its pending completion journal and is not published"}])
    );
    assert!(
        fixture.search(&["pending"])["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture.search(&["failed"])["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture.search(&["journaled"])["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn limit_reports_truncation_separately_from_an_incomplete_scan() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    for index in 1..=3 {
        event(&a, &format!("event-{index}.json"), "repeated", index);
    }
    let found = fixture.search(&["repeated", "--limit", "2"]);
    assert_eq!(found["limit"], 2);
    assert_eq!(found["hits"].as_array().unwrap().len(), 2);
    assert_eq!(found["hits"][0]["event_id"], "event-3.json");
    assert_eq!(found["truncated"], true);
    assert_eq!(found["incomplete"], false);

    let exact = fixture.search(&["repeated", "--limit", "3"]);
    assert_eq!(exact["truncated"], false);
}

#[test]
fn corrupt_records_make_the_search_incomplete_without_hiding_valid_hits() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    event(&a, "event-1.json", "valid needle result", 3);
    fs::write(a.join("events/event-2.json"), "not JSON").unwrap();
    let broken = fixture.session("session-broken", "claude", &fixture.workspace_a);
    event(
        &broken,
        "event-1.json",
        "needle behind a broken manifest",
        4,
    );
    fs::write(broken.join("manifest.json"), "broken manifest").unwrap();
    let before = files(fixture.root.path());

    let found = fixture.search(&["needle"]);
    assert_eq!(found["ok"], true);
    assert_eq!(found["incomplete"], true);
    assert_eq!(sessions(&found), ["session-a"]);
    let reasons = found["incomplete_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 2, "{found}");
    assert!(reasons.iter().any(|reason| reason["session"] == "session-a"
        && reason["reason"].as_str().unwrap().contains("event-2.json")));
    assert!(
        reasons
            .iter()
            .any(|reason| reason["session"] == "session-broken")
    );
    assert_eq!(files(fixture.root.path()), before);

    // Zero hits in an incomplete scan is never presented as "no results".
    let text = fixture.run(&["search", "absent"]);
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(!stdout.contains("no results"), "{stdout}");
    assert!(stdout.contains("incomplete"), "{stdout}");
    assert!(stdout.contains("session-broken"), "{stdout}");
}

#[test]
fn busy_session_is_reported_as_incomplete_not_empty() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    event(&a, "event-1.json", "locked needle", 3);
    let before = files(fixture.root.path());
    let lock_path = a.join("turn.claim.lock");
    let lock = fs::File::create(&lock_path).unwrap();
    lock.lock().unwrap();
    let started = std::time::Instant::now();
    let found = fixture.search(&["needle"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(found["incomplete"], true);
    assert!(found["hits"].as_array().unwrap().is_empty());
    assert_eq!(found["incomplete_reasons"][0]["session"], "session-a");
    assert!(
        found["incomplete_reasons"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("changing")
    );
    // Windows locks are mandatory, so release the fixture's own lock before comparing.
    drop(lock);
    fs::remove_file(&lock_path).unwrap();
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn matching_is_case_insensitive_and_excerpts_are_sanitised() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    let long = format!(
        "{}\x1b[31mNEEDLE\x07 here{}",
        "prefix ".repeat(100),
        " suffix".repeat(100)
    );
    event(&a, "event-1.json", &long, 3);
    event(&a, "event-2.json", "Straße École", 4);

    let found = fixture.search(&["needle"]);
    let excerpt = found["hits"][0]["excerpt"].as_str().unwrap();
    assert!(
        excerpt.contains("\\u{1b}[31mNEEDLE\\u{7} here"),
        "{excerpt}"
    );
    assert!(!excerpt.contains('\x1b'));
    assert!(
        excerpt.starts_with('…') && excerpt.ends_with('…'),
        "{excerpt}"
    );
    assert!(excerpt.chars().count() <= 200, "{excerpt}");
    assert!(!excerpt.contains(&"prefix ".repeat(50)));

    // Unicode lowercase on both sides: "STRAßE" and "ÉCOLE" match, but "STRASSE"
    // would need case folding and does not.
    let unicode = fixture.search(&["STRAßE écolE"]);
    assert_eq!(sessions(&unicode), ["session-a"]);
    assert_eq!(unicode["hits"][0]["excerpt"], "Straße École");
    assert_eq!(sessions(&fixture.search(&["ÉCOLE"])), ["session-a"]);
    assert!(
        fixture.search(&["STRASSE"])["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let text = fixture.run(&["search", "NEEDLE"]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    let line = stdout.lines().next().unwrap();
    assert_eq!(line.split('\t').count(), 5, "{line}");
    assert!(
        line.starts_with("session-a\tclaude\t3\tevent-1.json\t"),
        "{line}"
    );
    assert!(!stdout.contains('\x1b'));
}

#[test]
fn the_byte_budget_test_aid_is_closed_to_users() {
    // `--max-bytes` exists only for tests: without the test gate the binary rejects it as
    // an unknown option, so no user can be told about a search budget the docs omit.
    let fixture = Fixture::new();
    let directory = fixture.session("session-a", "claude", &fixture.workspace_a);
    event(&directory, "event-1.json", "needle", 1);
    let output = fixture.run_as_user(&["search", "needle", "--max-bytes", "1", "--json"]);
    assert!(!output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ok"], false, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("unknown search option: --max-bytes"),
        "{body}"
    );
    // Opened, the aid still cannot raise the budget above the documented limit.
    let output = fixture.run(&["search", "needle", "--max-bytes", "67108865", "--json"]);
    assert!(!output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("--max-bytes must be between 1 and 67108864"),
        "{body}"
    );
    let found = fixture.run_as_user(&["search", "needle", "--json"]);
    assert!(found.status.success());
}

#[test]
fn argument_and_state_errors_are_structured_with_json() {
    let fixture = Fixture::new();
    for args in [
        vec!["search"],
        vec!["search", "   "],
        vec!["search", "x", "--workspace", ".", "--all-workspaces"],
        vec!["search", "x", "--limit", "0"],
        vec!["search", "x", "--limit", "201"],
        vec!["search", "x", "--provider", "unknown"],
        vec!["search", "x", "--bogus"],
        vec!["search", "x", "--max-bytes", "0"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }

    // With --json, an argument error is still the documented structured failure.
    for (args, query, message) in [
        (
            vec!["search", "   ", "--json"],
            Value::Null,
            "non-empty query",
        ),
        (
            vec![
                "search",
                "x",
                "--workspace",
                ".",
                "--all-workspaces",
                "--json",
            ],
            json!("x"),
            "only one of --workspace or --all-workspaces",
        ),
        (
            vec!["search", "x", "--json", "--limit", "0"],
            json!("x"),
            "between 1 and 200",
        ),
        (
            vec!["search", "x", "--limit", "201", "--json"],
            json!("x"),
            "between 1 and 200",
        ),
        (
            vec!["search", "x", "--max-bytes", "0", "--json"],
            json!("x"),
            "--max-bytes must be between",
        ),
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        let body: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: {:?}", String::from_utf8_lossy(&output.stdout)));
        assert_eq!(body["schema_version"], 1, "{args:?}");
        assert_eq!(body["ok"], false, "{args:?}");
        assert_eq!(body["query"], query, "{args:?}");
        assert_eq!(body["hits"], json!([]), "{args:?}");
        assert!(
            body["error"].as_str().unwrap().contains(message),
            "{args:?}: {body}"
        );
    }

    let missing_root = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["search", "anything", "--all-workspaces", "--json"])
        .env(
            "AGENT_BRIDGE_NATIVE_STATE_DIR",
            fixture.root.path().join("never-created"),
        )
        .output()
        .unwrap();
    let empty = success(missing_root);
    assert_eq!(empty["hits"], json!([]));
    assert_eq!(empty["incomplete"], false);
    assert_eq!(empty["scanned"]["sessions"], 0);

    let file_root = fixture.root.path().join("not-a-directory");
    fs::write(&file_root, "x").unwrap();
    let unreadable = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["search", "anything", "--all-workspaces", "--json"])
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &file_root)
        .output()
        .unwrap();
    assert!(!unreadable.status.success());
    let body: Value = serde_json::from_slice(&unreadable.stdout).unwrap();
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().unwrap().contains("state root"));
}

fn reasons_for<'a>(value: &'a Value, session: Option<&str>) -> Vec<&'a str> {
    value["incomplete_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|reason| reason["session"] == session.map_or(Value::Null, Value::from))
        .map(|reason| reason["reason"].as_str().unwrap())
        .collect()
}

#[test]
fn corrupt_receipts_make_the_index_incomplete_and_suppress_legacy_hits() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    receipt(&a, "123-456-0", "request-readable", "event-1.json");
    event(&a, "event-1.json", "needle with a readable receipt", 3);
    // Two events lost their receipt mapping: one receipt is corrupt, one never existed.
    fs::write(a.join("requests/123-457-1.json"), "not JSON").unwrap();
    event(&a, "event-2.json", "needle whose receipt is corrupt", 4);
    event(&a, "event-3.json", "needle without any receipt", 5);
    // A fully readable index still reports legacy events.
    let legacy = fixture.session("session-legacy", "claude", &fixture.workspace_a);
    event(&legacy, "event-1.json", "needle legacy", 6);
    let before = files(fixture.root.path());

    let found = fixture.search(&["needle"]);
    assert_eq!(found["incomplete"], true, "{found}");
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{found}");
    assert_eq!(hits[0]["session"], "session-legacy");
    assert_eq!(hits[0]["request_id"], Value::Null);
    assert_eq!(hits[1]["session"], "session-a");
    assert_eq!(hits[1]["request_id"], "request-readable");
    assert!(
        hits.iter()
            .all(|hit| hit["session"] != "session-a" || hit["request_id"] != Value::Null),
        "{found}"
    );
    let reasons = reasons_for(&found, Some("session-a"));
    assert_eq!(reasons.len(), 1, "{found}");
    assert!(
        reasons[0].starts_with("request index incomplete: 1 unreadable receipt(s)"),
        "{}",
        reasons[0]
    );
    assert!(reasons[0].contains("2 event(s) without a readable receipt skipped"));
    assert!(reasons_for(&found, Some("session-legacy")).is_empty());
    // Skipped events are not counted as examined.
    assert_eq!(found["scanned"]["events"], 2);
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn unreadable_request_directory_is_reported_not_treated_as_legacy() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    event(&a, "event-1.json", "needle without an index", 3);
    fs::write(a.join("requests"), "not a directory").unwrap();
    let before = files(fixture.root.path());

    let found = fixture.search(&["needle"]);
    assert_eq!(found["incomplete"], true, "{found}");
    assert_eq!(found["hits"], json!([]));
    assert_eq!(found["scanned"]["sessions"], 1);
    assert_eq!(found["scanned"]["events"], 0);
    let reasons = reasons_for(&found, Some("session-a"));
    assert_eq!(reasons.len(), 1, "{found}");
    assert!(
        reasons[0].starts_with("request index incomplete: failed to read Bridge requests"),
        "{}",
        reasons[0]
    );
    assert!(reasons[0].ends_with("1 event(s) without a readable receipt skipped"));
    assert_eq!(files(fixture.root.path()), before);
}

#[test]
fn byte_budget_is_checked_before_an_event_is_read() {
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    let small = event(&a, "event-1.json", "needle small", 3);
    let small_len = serde_json::to_vec_pretty(&small).unwrap().len();
    let big = event(
        &a,
        "event-2.json",
        &format!("needle {}", "x".repeat(4096)),
        4,
    );
    let big_len = serde_json::to_vec_pretty(&big).unwrap().len();
    assert!(big_len > small_len * 2);
    let before = files(fixture.root.path());

    // Alone, an event larger than the whole budget is never read and stops the scan even
    // though it is the last event.
    let budget = (big_len - 1).to_string();
    let found = fixture.search(&["needle", "--max-bytes", &budget]);
    assert_eq!(found["incomplete"], true, "{found}");
    assert_eq!(sessions(&found), ["session-a"]);
    assert_eq!(found["hits"][0]["event_id"], "event-1.json");
    assert_eq!(found["scanned"]["events"], 1);
    let reasons = reasons_for(&found, None);
    assert_eq!(reasons.len(), 1, "{found}");
    assert!(
        reasons[0].contains(&format!("byte budget of {budget} bytes exhausted")),
        "{}",
        reasons[0]
    );
    assert!(
        reasons[0].contains(&format!("session-a/event-2.json is {big_len} bytes")),
        "{}",
        reasons[0]
    );

    // The running total counts: the budget fits the big event alone but not after the
    // small one has been read.
    let total = (big_len + small_len - 1).to_string();
    let found = fixture.search(&["needle", "--max-bytes", &total]);
    assert_eq!(found["incomplete"], true, "{found}");
    assert_eq!(found["scanned"]["events"], 1);
    assert!(reasons_for(&found, None)[0].contains("byte budget"));

    // With both fitting exactly, the scan completes.
    let exact = (big_len + small_len).to_string();
    let found = fixture.search(&["needle", "--max-bytes", &exact]);
    assert_eq!(found["incomplete"], false, "{found}");
    assert_eq!(found["hits"].as_array().unwrap().len(), 2);
    assert_eq!(files(fixture.root.path()), before);

    let text = fixture.run(&["search", "needle", "--max-bytes", &budget]);
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(stdout.contains("incomplete"), "{stdout}");
    assert!(stdout.contains("byte budget"), "{stdout}");
}

#[test]
fn damaged_session_directories_are_incomplete_not_empty() {
    let fixture = Fixture::new();
    let file = fixture.session("session-file", "claude", &fixture.workspace_a);
    fs::remove_dir(file.join("events")).unwrap();
    fs::write(file.join("events"), "not a directory").unwrap();
    let missing = fixture.session("session-missing", "claude", &fixture.workspace_a);
    fs::remove_dir(missing.join("events")).unwrap();
    let no_status = fixture.session("session-nostatus", "claude", &fixture.workspace_a);
    event(
        &no_status,
        "event-1.json",
        "needle behind a missing status",
        3,
    );
    fs::remove_file(no_status.join("status.json")).unwrap();
    let manifest_dir = fixture.session("session-manifest", "claude", &fixture.workspace_a);
    event(
        &manifest_dir,
        "event-1.json",
        "needle behind an unreadable manifest",
        4,
    );
    fs::remove_file(manifest_dir.join("manifest.json")).unwrap();
    fs::create_dir(manifest_dir.join("manifest.json")).unwrap();
    let healthy = fixture.session("session-ok", "claude", &fixture.workspace_a);
    event(&healthy, "event-1.json", "needle healthy", 5);
    let before = files(fixture.root.path());

    let found = fixture.search(&["needle"]);
    assert_eq!(found["ok"], true);
    assert_eq!(found["incomplete"], true, "{found}");
    assert_eq!(sessions(&found), ["session-ok"]);
    assert_eq!(found["scanned"]["events"], 1);
    assert_eq!(
        reasons_for(&found, Some("session-file")),
        ["events is not a directory"]
    );
    assert_eq!(
        reasons_for(&found, Some("session-missing")),
        ["events directory is missing"]
    );
    let no_status = reasons_for(&found, Some("session-nostatus"));
    assert_eq!(no_status.len(), 1, "{found}");
    assert!(
        no_status[0].contains("no status record"),
        "{}",
        no_status[0]
    );
    let manifest = reasons_for(&found, Some("session-manifest"));
    assert_eq!(manifest.len(), 1, "{found}");
    assert!(manifest[0].contains("manifest.json"), "{}", manifest[0]);
    assert!(reasons_for(&found, Some("session-ok")).is_empty());
    assert_eq!(files(fixture.root.path()), before);

    let text = fixture.run(&["search", "absent"]);
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(!stdout.contains("no results"), "{stdout}");
    assert!(
        stdout.contains("session-file: events is not a directory"),
        "{stdout}"
    );
    assert!(
        stdout.contains("session-missing: events directory is missing"),
        "{stdout}"
    );
}

/// The event exactly as a completion writes it: pretty JSON in record field order, the
/// byte form that publication compares with the journal.
fn journaled_event_text(name: &str, message: &str) -> String {
    format!(
        "{{\n  \"provider\": \"claude\",\n  \"message\": \"{message}\",\n  \"error\": null,\n  \"provider_session_id\": \"native-session\",\n  \"turn_id\": \"{name}\",\n  \"created_unix_ms\": 5\n}}"
    )
}

#[test]
fn publication_checks_read_journaled_events_within_the_byte_budget() {
    // A completion that wrote its journal and event and still holds its claim: deciding
    // whether the event is published means comparing the whole record with the journal.
    let fixture = Fixture::new();
    let a = fixture.session("session-a", "claude", &fixture.workspace_a);
    let message = format!("needle {}", "x".repeat(4200));
    let text = journaled_event_text("event-1.json", &message);
    fs::write(a.join("events").join("event-1.json"), &text).unwrap();
    receipt(&a, "1-1-1", "request-1", "event-1.json");
    fs::write(a.join("turn.claim"), "1-1-1").unwrap();
    write(
        &a.join("turn.completion.json"),
        &json!({"schema": 1, "claim_token": "1-1-1", "event_file": "event-1.json",
            "event": serde_json::from_str::<Value>(&text).unwrap(),
            "status_error": null, "status_state": "ready"}),
    );
    let size = text.len();

    // With one byte of budget the comparison cannot run; the record is not read past
    // the budget, and the scan reports the record it could not verify.
    let starved = fixture.search(&["needle", "--max-bytes", "1"]);
    assert_eq!(starved["hits"], json!([]));
    assert_eq!(starved["incomplete"], true, "{starved}");
    assert_eq!(starved["scanned"]["events"], 0, "{starved}");
    let reasons = starved["incomplete_reasons"].to_string();
    assert!(
        reasons.contains(&format!(
            "session-a/event-1.json is {size} bytes with 1 bytes remaining"
        )),
        "{reasons}"
    );

    // The full budget verifies the record, charges it once, and searches it.
    let found = fixture.search(&["needle", "--max-bytes", &size.to_string()]);
    assert_eq!(sessions(&found), ["session-a"]);
    assert_eq!(found["hits"][0]["request_id"], "request-1");
    assert_eq!(found["incomplete"], false, "{found}");
    assert_eq!(found["scanned"]["events"], 1, "{found}");
}
