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
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.workspace_a, args)
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
    assert_eq!(found["incomplete"], false);
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["event_id"], "event-4.json");
    assert_eq!(found["scanned"]["events"], 2);
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
    assert!(
        excerpt.chars().count() <= 200 + "\\u{1b}\\u{7}".len(),
        "{excerpt}"
    );
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
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
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
