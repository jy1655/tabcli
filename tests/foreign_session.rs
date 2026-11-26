//! A Session whose own Records positively say it belongs to another OS, as a state directory
//! copied between machines holds it: its bound active handle names a kind this OS does not run
//! (or, for Herdr, a host born on another OS), its consumption marker says it consumed such a
//! kind, or its bound Owner record has another OS's identity.
//!
//! For such a Session TAB does nothing that depends on this machine's terminals or processes,
//! and everything it does from Records alone is as before. Every public command that
//! `describe` lists is classified below and run against every fixture:
//! - `tell`, `screen` and doctor's probe give the one refusal where they would address the
//!   terminal, in one order (closed, then unbound, then another OS);
//! - the explicit close settles a copy whose Records show it ended and refuses any other, and
//!   prune removes a closed copy; a settlement followed by prune removes the same fixture;
//! - every other command gives its answer from Records, as before.
//!
//! No command connects to the Herdr Core or tmux server the fixture's handle names, or reports
//! a local observation of the owner (a PID-only owner is this test's own, alive here). A
//! command added later that is on no list fails the test. The same on macOS, Linux and Windows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// Commands that would address the session's terminal: they give the one refusal there.
const REFUSES: &[&str] = &["tell", "screen", "doctor"];
/// Commands that settle a copy from its Records: the explicit close and prune.
const SETTLES: &[&str] = &["close-session", "prune-sessions"];
/// Commands that act from Records alone, exactly as before.
const RECORDS: &[&str] = &[
    "hold", "cancel", "reopen", "status", "inspect", "result", "wait", "search", "sessions",
];
/// Commands that take no session.
const NO_SESSION: &[&str] = &["describe", "ask", "self-test", "consent", "settings"];

const ID: &str = "session-foreign";
const CLAIM: &str = "7-7-7";
const REQUEST: &str = "request-foreign";
const FAILURE: &str = "the provider failed";

// This OS's name.
fn here() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Windows"
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Owner {
    /// The Owner record that the terminal's own OS writes, bound, for a PID absent here.
    Full,
    /// A bound PID-only record whose PID is this test's own, alive here: any local observation
    /// of it would report a live process.
    PidOnly,
    Absent,
}

/// Where the fixture's handle is recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Handle {
    /// The active handle, bound to the session.
    Active,
    /// Claimed by a close interrupted before it consumed it.
    Claimed,
    /// Consumed: a marker that says so and keeps its kind, beside a leftover Herdr handle.
    Consumed,
}

#[derive(Clone, Debug)]
struct Case {
    name: String,
    /// The handle's kind as its record names it.
    kind: &'static str,
    terminal: Value,
    handle: Handle,
    origin: &'static str,
    owner: Owner,
    state: &'static str,
    closed: bool,
    expired_launch: bool,
    working: bool,
    /// What the Records name: the start of the one refusal.
    head: String,
}

impl Case {
    // Whether the explicit close settles it: closed, already consumed, or ended; an expired
    // launch is settled as failed from its records first, as before.
    fn settles(&self) -> bool {
        self.closed
            || self.handle == Handle::Consumed
            || self.expired_launch
            || ["exited", "failed"].contains(&self.state)
    }

    // Whether an active handle is recorded (a consumed copy keeps a leftover one).
    fn active(&self) -> bool {
        self.handle == Handle::Active || (self.handle == Handle::Consumed && !self.closed)
    }
}

fn handle(terminal: &str) -> Value {
    json!({"terminal": terminal, "session_id": "surface", "managed_session_id": ID})
}

// A Herdr handle born on `origin`, naming a socket in the fixture root.
fn herdr(origin: &str) -> Value {
    let birth = match origin {
        "macOS" => json!({"host_start_seconds": 1, "host_start_microseconds": 2}),
        "Linux" => json!({"linux_birth": {
            "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7}}),
        _ => json!({"host_creation_time": 1}),
    };
    let mut core = json!({"socket": "HERDR_SOCKET", "core_id": "core-a", "created_ms": 1,
        "host_pid": 4});
    for (key, value) in birth.as_object().unwrap() {
        core[key] = value.clone();
    }
    json!({"terminal": "herdr", "session_id": "t7", "managed_session_id": ID, "herdr_core": core})
}

// Every fixture of a session of another OS this host can be given.
fn cases() -> Vec<Case> {
    let os = here();
    let mut cases = Vec::new();
    for (terminal, name, origin) in [
        ("iterm2", "iTerm2", "macOS"),
        ("apple-terminal", "Terminal.app", "macOS"),
        ("ghostty", "Ghostty", "macOS"),
        ("warp", "Warp", "macOS"),
        ("wezterm", "WezTerm", "macOS"),
        ("windows-console", "Windows Console", "Windows"),
        ("tmux", "tmux", "Linux"),
    ] {
        if origin == os {
            continue;
        }
        let base = |label: &str, state: &'static str, owner| Case {
            name: format!("{terminal}, {label}"),
            kind: terminal,
            terminal: handle(terminal),
            handle: Handle::Active,
            origin,
            owner,
            state,
            closed: false,
            expired_launch: false,
            working: false,
            head: format!("{name} is not available on {os}"),
        };
        for (label, owner) in [
            ("ready, owner", Owner::Full),
            ("ready, PID-only owner", Owner::PidOnly),
            ("ready, no owner", Owner::Absent),
        ] {
            cases.push(base(label, "ready", owner));
        }
        cases.push(Case {
            expired_launch: true,
            ..base("expired launch, no owner", "launching", Owner::Absent)
        });
        cases.push(Case {
            working: true,
            ..base("working", "working", Owner::Full)
        });
        // A claimed handle is no evidence: the Owner record names the OS.
        let owner_head = format!("a session recorded on {origin} is not available on {os}");
        cases.push(Case {
            handle: Handle::Claimed,
            head: owner_head.clone(),
            ..base("working, interrupted close", "working", Owner::Full)
        });
        cases.push(base("exited", "exited", Owner::Full));
        cases.push(base("failed, PID-only owner", "failed", Owner::PidOnly));
        cases.push(Case {
            handle: Handle::Claimed,
            head: owner_head,
            ..base("failed, interrupted close", "failed", Owner::Full)
        });
        cases.push(Case {
            handle: Handle::Consumed,
            terminal: json!({"consumed": true, "terminal": terminal}),
            ..base("exited, handle consumed", "exited", Owner::PidOnly)
        });
        cases.push(Case {
            handle: Handle::Consumed,
            closed: true,
            terminal: json!({"consumed": true, "terminal": terminal}),
            ..base("closed copy", "closed", Owner::Full)
        });
    }
    for origin in ["macOS", "Linux", "Windows"] {
        if origin == os {
            continue;
        }
        for state in ["ready", "exited"] {
            cases.push(Case {
                name: format!("herdr born on {origin}, {state}, no owner"),
                kind: "herdr",
                terminal: herdr(origin),
                handle: Handle::Active,
                origin,
                owner: Owner::Absent,
                state,
                closed: false,
                expired_launch: false,
                working: false,
                head: format!("a session recorded on {origin} is not available on {os}"),
            });
        }
    }
    cases
}

// The Owner record that `os`'s writer records, bound to the session.
fn owner(os: &str, pid: u32) -> Value {
    let mut owner = json!({"pid": pid, "managed_session_id": ID});
    match os {
        "macOS" => {
            owner["terminal_tty"] = json!("/dev/ttys009");
            owner["terminal_tty_device"] = json!(9);
            owner["process_start_seconds"] = json!(1);
            owner["process_start_microseconds"] = json!(2);
            owner["process_group"] = json!(pid);
            owner["terminal_process_group"] = json!(pid);
        }
        "Linux" => {
            owner["terminal_tty"] = json!("/dev/pts/9");
            owner["terminal_tty_device"] = json!(9);
            owner["linux_birth"] =
                json!({"boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7});
            owner["process_group"] = json!(pid);
            owner["terminal_process_group"] = json!(pid);
        }
        _ => {
            owner["windows_process_identity"] = json!({
                "creation_time": 1,
                "executable_path": r"C:\Program Files\PowerShell\7\pwsh.exe"
            });
        }
    }
    owner
}

// A PID that no process holds here: a child that has exited and been reaped.
fn absent_pid() -> u32 {
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    #[cfg(not(windows))]
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

// The endpoints a fixture's handle names, listening, so that any connection is counted: the
// Herdr Core socket its Herdr handle records, and the tmux server socket of the session.
#[cfg(unix)]
struct Endpoints(Vec<std::os::unix::net::UnixListener>);

#[cfg(unix)]
impl Endpoints {
    fn listen(paths: &[PathBuf]) -> Self {
        Self(
            paths
                .iter()
                .map(|path| {
                    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
                    listener.set_nonblocking(true).unwrap();
                    listener
                })
                .collect(),
        )
    }

    // The connections made since the last count.
    fn connections(&self) -> usize {
        self.0
            .iter()
            .map(|listener| std::iter::from_fn(|| listener.accept().ok()).count())
            .sum()
    }
}

// Windows has no Unix socket listener here; its endpoints are named pipes no fixture names.
#[cfg(not(unix))]
struct Endpoints;

#[cfg(not(unix))]
impl Endpoints {
    fn listen(_: &[PathBuf]) -> Self {
        Self
    }

    fn connections(&self) -> usize {
        0
    }
}

struct Fixture {
    root: tempfile::TempDir,
    endpoints: Endpoints,
}

impl Fixture {
    fn new(case: &Case) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(ID);
        fs::create_dir_all(directory.join("events")).unwrap();
        fs::create_dir_all(directory.join("requests")).unwrap();
        // The lifecycle locks exist, as in every session, so that taking one writes nothing.
        for lock in ["turn.claim.lock", "status.lock"] {
            fs::write(directory.join(lock), b"").unwrap();
        }
        support::write(
            &directory.join("manifest.json"),
            &support::manifest(
                ID,
                "pi",
                "pi".as_ref(),
                "1.5.0",
                &root.path().canonicalize().unwrap(),
                "copied from another OS",
            ),
        );
        let error = (case.state == "failed").then_some(FAILURE);
        support::write(
            &directory.join("status.json"),
            &support::status(case.state, 2, error),
        );
        if case.closed {
            support::write(
                &directory.join("closed.json"),
                &support::status("closed", 2, None),
            );
        }
        let socket = root.path().join("herdr-core.sock");
        let mut terminal = case.terminal.clone();
        if terminal["herdr_core"].is_object() {
            terminal["herdr_core"]["socket"] = json!(socket);
        }
        match case.handle {
            Handle::Active => support::write(&directory.join("terminal.json"), &terminal),
            Handle::Claimed => support::write(&directory.join("terminal.closing.json"), &terminal),
            Handle::Consumed => {
                support::write(&directory.join("terminal.closed.json"), &terminal);
                if !case.closed {
                    // A leftover handle of a kind this OS runs: the marker's kind is kept.
                    support::write(&directory.join("terminal.json"), &handle("herdr"));
                }
            }
        }
        match case.owner {
            Owner::Full => support::write(
                &directory.join("native-session.json"),
                &owner(case.origin, absent_pid()),
            ),
            Owner::PidOnly => support::write(
                &directory.join("native-session.json"),
                &json!({"pid": std::process::id(), "managed_session_id": ID}),
            ),
            Owner::Absent => {}
        }
        if case.expired_launch {
            fs::write(directory.join("turn.claim"), CLAIM).unwrap();
            support::write(
                &directory.join("launch.json"),
                &json!({"schema": 1, "claim_token": CLAIM, "deadline_unix_ms": 1,
                    "phase": "pending"}),
            );
        }
        if case.working {
            fs::write(directory.join("turn.claim"), CLAIM).unwrap();
            support::write(
                &directory.join("requests").join(format!("{CLAIM}.json")),
                &support::receipt(REQUEST, CLAIM, "event-7.json"),
            );
        }
        let endpoints = Endpoints::listen(&[socket, directory.join("tmux.sock")]);
        Self { root, endpoints }
    }

    fn directory(&self) -> PathBuf {
        self.root.path().join(ID)
    }

    // The one refusal for this fixture's session, naming its records.
    fn refusal(&self, head: &str) -> String {
        format!(
            "{head}: TAB on this machine cannot judge or end this session; once its records \
             show it ended, an explicit close settles it (its records are in {})",
            self.directory().display()
        )
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            // No Herdr Core of this machine is ever addressed: an endpoint resolution would
            // name this relative socket, and no test output may.
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HC_SESSION")
            .env("HC_SOCKET", "relative-herdr-core.sock")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn run(&self, command: &str) -> Output {
        self.output(
            &arguments(command)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
    }

    // The records, without the listening sockets.
    fn records(&self) -> Vec<support::Entry> {
        fn walk(directory: &Path, entries: &mut Vec<support::Entry>) {
            let mut children: Vec<_> = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            children.sort();
            for path in children {
                if path
                    .extension()
                    .is_some_and(|extension| extension == "sock")
                {
                    continue;
                }
                if path.is_dir() {
                    entries.push((path.clone(), None));
                    walk(&path, entries);
                } else {
                    entries.push((path.clone(), Some(fs::read(&path).unwrap())));
                }
            }
        }
        let mut entries = Vec::new();
        walk(self.root.path(), &mut entries);
        entries
    }

    // Moves the closed status records back in time, as days passing would, so that prune's
    // retention cutoff covers a settlement just made.
    fn age_closed_records(&self) {
        for name in ["status.json", "closed.json"] {
            let path = self.directory().join(name);
            let mut status = read_json(&path);
            status["updated_unix_ms"] = json!(2);
            support::write(&path, &status);
        }
    }
}

fn reply(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

// A record as JSON; null when it is missing or unreadable, so that a departure is reported.
fn read_json(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

// The command's arguments against this fixture's session.
fn arguments(command: &str) -> Vec<String> {
    let address = format!("{ID}/{REQUEST}");
    let args: Vec<&str> = match command {
        "tell" => vec!["tell", ID, "--prompt", "hello", "--json"],
        "hold" => vec!["hold", ID, "--json"],
        "cancel" => vec!["cancel", ID, "--json"],
        "reopen" => vec!["reopen", ID, "--prompt", "continue", "--json"],
        "screen" => vec!["screen", ID, "--json"],
        "close-session" => vec!["close-session", ID, "--explicit", "--json"],
        "status" => vec!["status", "--all-workspaces", "--all", "--json"],
        "inspect" => vec!["inspect", ID, "--json"],
        "result" => vec!["result", ID, "--latest", "--json"],
        "wait" => vec!["wait", &address, "--timeout-secs", "1", "--json"],
        "search" => vec!["search", "hello", "--all-workspaces", "--json"],
        "doctor" => vec!["doctor", ID, "--probe", "--json"],
        "sessions" => vec!["sessions", "--json"],
        "prune-sessions" => vec![
            "prune-sessions",
            "--closed-before-days",
            "1",
            "--explicit",
            "--json",
        ],
        other => panic!("no arguments for {other}"),
    };
    args.into_iter().map(str::to_owned).collect()
}

// Every owner observation a command reports: none may come from this machine.
fn owner_observations(value: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    fn walk(value: &Value, found: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "process_alive" || key == "owner_process_alive" {
                        found.push(child.clone());
                    }
                    walk(child, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            _ => {}
        }
    }
    walk(value, &mut found);
    found
}

// One check of a doctor report.
fn doctor_check(value: &Value, id: &str) -> Value {
    value["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["id"] == id))
        .cloned()
        .unwrap_or(Value::Null)
}

// The error text of a failed command: its JSON `error`, or the last cause on stderr.
fn failure(output: &Output) -> String {
    if let Some(error) = reply(output)["error"].as_str() {
        return error.to_owned();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

// The marker of an expected answer that is the read error of a missing record: the record's
// name and the error code (`os error 2`) are the same on every OS, the OS's wording is not.
const MISSING: &str = "missing record: ";

fn missing_record(record: &str) -> String {
    format!("{MISSING}{record}")
}

// What a command answers for this fixture. `Ok(None)` is a success whose details the caller
// checks; `Err(text)` a failure whose error is `text` or ends with it.
fn expected(command: &str, case: &Case, fixture: &Fixture) -> Result<Option<Value>, String> {
    let refusal = fixture.refusal(&case.head);
    let state = if case.closed { "closed" } else { case.state };
    match command {
        // The proof refuses an active handle; without one, `tell` reads no handle, as before.
        "tell" if case.active() => Err(refusal),
        // The OS words the missing file its own way: only the record and the error code are
        // the same everywhere (`missing_record`).
        "tell" => Err(missing_record("terminal.json")),
        "screen" if case.closed => Err(format!(
            "session {ID} is closed; its terminal surface is not read"
        )),
        "screen" if case.handle == Handle::Active => Err(refusal),
        "screen" => Err(format!(
            "the terminal surface of session {ID} is closed or being closed"
        )),
        "hold" if case.closed => {
            Err("the session is closed; a hold cannot be set or released".to_owned())
        }
        "hold" => Ok(Some(json!({"held": true}))),
        "cancel" if case.working => Err(
            "this session was started by an extension without cancel support; start a new session"
                .to_owned(),
        ),
        "cancel" if state == "working" => {
            Err(format!("session {ID} has no active claim to cancel"))
        }
        "cancel" => Err(format!(
            "session {ID} is {state}; cancel requires the running or working state"
        )),
        "reopen" if case.closed => Err(format!(
            "reopen refused (source-identity-missing): session {ID} has no pi event that records a provider session id; a session whose only turn failed cannot be reopened"
        )),
        "reopen" => Err(format!(
            "reopen refused (source-not-closed): session {ID} is {state}; reopen requires a session closed with its closed tombstone"
        )),
        "result" => Err("no published result is available for this selection".to_owned()),
        "wait" if case.working => {
            Err("waiting timed out; no request was cancelled or resent".to_owned())
        }
        "wait" => Err(format!(
            "no such TAB request: {REQUEST}; legacy events have no request mapping"
        )),
        "close-session" if !case.settles() => Err(refusal),
        "prune-sessions" if case.closed => Ok(Some(json!({"pruned": [ID]}))),
        "prune-sessions" => Ok(Some(json!({"pruned": []}))),
        _ => Ok(None),
    }
}

#[test]
fn every_public_command_is_classified_for_a_session_of_another_os() {
    let probe = Fixture::new(&cases()[0]);
    // The endpoint counter counts: one connection to each endpoint a fixture listens on.
    #[cfg(unix)]
    {
        for path in [
            probe.root.path().join("herdr-core.sock"),
            probe.directory().join("tmux.sock"),
        ] {
            drop(std::os::unix::net::UnixStream::connect(path).unwrap());
        }
        assert_eq!(probe.endpoints.connections(), 2);
        assert_eq!(probe.endpoints.connections(), 0);
    }
    let described = reply(&probe.output(&["describe", "--json"]));
    let names: Vec<String> = described["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!names.is_empty());
    for name in &names {
        let classes = [REFUSES, SETTLES, RECORDS, NO_SESSION]
            .iter()
            .filter(|class| class.contains(&name.as_str()))
            .count();
        assert_eq!(
            classes, 1,
            "{name} must be on exactly one list: does it address the terminal, settle from records, act from records alone, or take no session?"
        );
    }
    for class in [REFUSES, SETTLES, RECORDS, NO_SESSION] {
        for name in class {
            assert!(
                names.iter().any(|listed| listed == name),
                "{name} is not public"
            );
        }
    }
}

#[test]
fn a_session_of_another_os_is_refused_settled_or_answered_from_its_records() {
    // Every departure is collected, so that one run names all of them.
    let mut failures = Vec::new();
    for case in cases() {
        for command in REFUSES.iter().chain(SETTLES).chain(RECORDS) {
            let fixture = Fixture::new(&case);
            let before = fixture.records();
            let output = fixture.run(command);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let mut check = |held: bool, what: &str| {
                if !held {
                    failures.push(format!("{}: {command}: {what}: {text}", case.name));
                }
            };
            check(
                fixture.endpoints.connections() == 0,
                "connected to the terminal's endpoint",
            );
            check(
                !text.contains("relative-herdr-core.sock"),
                "addressed a Herdr Core",
            );
            let value = reply(&output);
            for observation in owner_observations(&value) {
                check(observation.is_null(), "reported a local owner observation");
            }
            match expected(command, &case, &fixture) {
                Err(answer) => {
                    check(!output.status.success(), "succeeded");
                    let error = failure(&output);
                    let matched = match answer.strip_prefix(MISSING) {
                        Some(record) => {
                            error.starts_with("failed to read ")
                                && error.contains(record)
                                && error.ends_with("(os error 2)")
                        }
                        None => error == answer || error.ends_with(&answer),
                    };
                    check(matched, &format!("not `{answer}`"));
                    if answer == fixture.refusal(&case.head) && *command != "close-session" {
                        check(
                            value["failure_kind"] == "unsupported",
                            "not an unsupported failure",
                        );
                    }
                }
                Ok(details) => {
                    check(output.status.success(), "failed");
                    check(!value.is_null(), "no JSON answer");
                    if let Some(Value::Object(details)) = details {
                        for (key, expected) in details {
                            check(
                                value[&key] == expected,
                                &format!("`{key}` is not {expected}"),
                            );
                        }
                    }
                }
            }
            // What may change: an expired launch is settled from its records by the commands
            // that converge, as before; `hold` writes its record; the close and prune settle.
            let after = fixture.records();
            match *command {
                "close-session" if case.settles() => {
                    let directory = fixture.directory();
                    let status = read_json(&directory.join("status.json"));
                    check(status["state"] == "closed", "status is not closed");
                    check(
                        read_json(&directory.join("closed.json"))["state"] == "closed",
                        "no closed record",
                    );
                    check(
                        !directory.join("terminal.json").exists()
                            && !directory.join("terminal.closing.json").exists(),
                        "a handle remains",
                    );
                    let marker = read_json(&directory.join("terminal.closed.json"));
                    check(
                        marker["consumed"] == true && marker["terminal"] == case.kind,
                        "the consumption marker does not keep the consumed kind",
                    );
                    let error = status["error"].as_str().unwrap_or_default().to_owned();
                    if !case.closed && case.handle != Handle::Consumed {
                        check(
                            error.contains(&format!(
                                "settled from its records alone ({})",
                                case.head
                            )),
                            "the closed status does not say how it was settled",
                        );
                    }
                    if case.state == "failed" {
                        check(error.contains(FAILURE), "the failure diagnosis was lost");
                    }
                    // The same copy, once its retention has passed, is pruned without a local
                    // observation of its owner.
                    fixture.age_closed_records();
                    let pruned = fixture.run("prune-sessions");
                    check(
                        reply(&pruned)["pruned"] == json!([ID]) && !directory.exists(),
                        "the settled copy was not pruned",
                    );
                    check(
                        fixture.endpoints.connections() == 0,
                        "prune connected to the terminal's endpoint",
                    );
                }
                "prune-sessions" if case.closed => {
                    check(!fixture.directory().exists(), "the closed copy remains");
                }
                "hold" if !case.closed => {
                    let others = |entries: Vec<support::Entry>| -> Vec<support::Entry> {
                        entries
                            .into_iter()
                            .filter(|(path, _)| !path.ends_with("hold.json"))
                            .collect()
                    };
                    check(
                        others(after) == others(before),
                        "changed a record besides the hold",
                    );
                }
                "tell" | "sessions" if case.expired_launch => {}
                _ => check(after == before, "changed the records"),
            }
            if *command == "doctor" {
                // The probe's answer: a missing active handle as before, then the closed one,
                // the one refusal otherwise.
                let answer = if !case.active() {
                    "no active terminal handle is recorded".to_owned()
                } else if case.closed {
                    format!("session {ID} is closed; its terminal surface is not read")
                } else {
                    fixture.refusal(&case.head)
                };
                check(
                    doctor_check(&value, "terminal_surface")["detail"]
                        == format!("Surface presence could not be verified: {answer}"),
                    "the probe did not give the one answer",
                );
            }
            if *command == "inspect" {
                // `tell`'s capability projects its ownership proof for an active handle.
                let tell = &value["capabilities"]["tell"];
                if case.handle == Handle::Active {
                    check(
                        *tell
                            == json!({"state": "refused", "reason": "surface_unsupported",
                                "detail": fixture.refusal(&case.head)}),
                        "the tell capability",
                    );
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// The same records get the same answer from every command that would address the terminal,
// in one order: a closed session first, then a handle that is not this session's, then a
// session of another OS. A legacy iTerm2 handle without a binding still counts as this
// session's there, though it is no evidence by itself.
#[test]
fn every_command_answers_the_same_records_in_one_order() {
    let os = here();
    let (terminal, origin) = if os == "Windows" {
        ("apple-terminal", "macOS")
    } else {
        ("windows-console", "Windows")
    };
    let ready = cases()
        .into_iter()
        .find(|case| {
            case.terminal["terminal"] == terminal
                && case.owner == Owner::Full
                && case.state == "ready"
        })
        .unwrap();
    assert_eq!(ready.origin, origin);

    // A handle bound to another session, beside an Owner record of another OS: the binding
    // answers before the OS.
    let mut unbound = ready.clone();
    unbound.terminal["managed_session_id"] = json!("session-other");
    let binding = format!("terminal handle belongs to managed session session-other, not {ID}");
    let fixture = Fixture::new(&unbound);
    let before = fixture.records();
    for command in ["tell", "screen", "close-session"] {
        let output = fixture.run(command);
        assert!(!output.status.success(), "{command}");
        let error = failure(&output);
        assert!(error.ends_with(&binding), "{command}: {error}");
    }
    let inspected = reply(&fixture.output(&["inspect", ID, "--json"]));
    assert_eq!(
        inspected["capabilities"]["tell"]["reason"], "surface_unbound",
        "{inspected}"
    );
    let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
    assert_eq!(
        doctor_check(&doctor, "terminal_surface")["detail"],
        format!("Surface presence could not be verified: {binding}")
    );
    assert_eq!(fixture.records(), before);

    // A legacy iTerm2 handle without a binding, beside an Owner record of macOS: on an OS
    // that does not run iTerm2 the OS the owner names answers.
    if os != "macOS" {
        let legacy = Case {
            terminal: json!({"terminal": "iterm2", "session_id": "surface"}),
            origin: "macOS",
            ..ready.clone()
        };
        let fixture = Fixture::new(&legacy);
        let refusal = fixture.refusal(&format!(
            "a session recorded on macOS is not available on {os}"
        ));
        for command in ["tell", "screen", "close-session"] {
            let error = failure(&fixture.run(command));
            assert!(error.ends_with(&refusal), "{command}: {error}");
        }
        let inspected = reply(&fixture.output(&["inspect", ID, "--json"]));
        assert_eq!(
            inspected["capabilities"]["tell"],
            json!({"state": "refused", "reason": "surface_unsupported", "detail": refusal})
        );
        let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
        assert_eq!(
            doctor_check(&doctor, "terminal_surface")["detail"],
            format!("Surface presence could not be verified: {refusal}")
        );
    }

    // A closed session whose bound handle of another OS remains: closed answers first.
    let stale = Case {
        state: "closed",
        ..ready
    };
    let fixture = Fixture::new(&stale);
    support::write(
        &fixture.directory().join("closed.json"),
        &support::status("closed", 2, None),
    );
    let before = fixture.records();
    for (command, answer) in [
        (
            "tell",
            format!("session {ID} is closed; its terminal is not addressed"),
        ),
        (
            "screen",
            format!("session {ID} is closed; its terminal surface is not read"),
        ),
    ] {
        assert_eq!(failure(&fixture.run(command)), answer, "{command}");
    }
    let inspected = reply(&fixture.output(&["inspect", ID, "--json"]));
    assert_eq!(
        inspected["capabilities"]["tell"],
        json!({"state": "refused", "reason": "state_not_ready",
            "detail": format!("session {ID} is closed; its terminal is not addressed")})
    );
    let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
    assert_eq!(
        doctor_check(&doctor, "terminal_surface")["detail"],
        format!(
            "Surface presence could not be verified: session {ID} is closed; its terminal surface is not read"
        )
    );
    assert_eq!(fixture.records(), before);
    // Its records alone finish the close, as for any closed session: the handle is consumed,
    // its kind kept, and no terminal is reached.
    let output = fixture.run("close-session");
    assert!(output.status.success(), "{}", failure(&output));
    assert!(!fixture.directory().join("terminal.json").exists());
    assert_eq!(
        read_json(&fixture.directory().join("terminal.closed.json"))["terminal"],
        terminal
    );
    assert_eq!(fixture.endpoints.connections(), 0);
}
