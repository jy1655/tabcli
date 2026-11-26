//! A Session whose own Records positively say it belongs to another OS, as a state directory
//! copied between machines holds it: its bound active handle states a kind this OS does not
//! run (or, for Herdr, a host born on another OS), its consumption marker says it consumed such
//! a kind, or its bound Owner record has another OS's identity.
//!
//! Only the explicit close, prune and Repair treat such a Session differently: the close
//! settles a copy whose Records show it ended and refuses any other, prune removes a closed
//! copy, and Repair (which `sessions`, `tell` and the close run) changes nothing from what this
//! machine's processes show. Every command `describe` lists is on one list below; a command
//! added later that is on no list fails the test.
//!
//! The explicit close, prune and the commands that run Repair are run twice against each
//! fixture: once with every recorded PID (the owner's, the provider's and the Herdr host's)
//! naming a process alive on this machine, and once naming one that is not. Their answers and
//! every record must be the same in both runs, so anything derived from a local process
//! observation fails the test without anyone listing where it is observed. The other commands
//! answer for a copy as for any session; nothing here asserts that they do not depend on local
//! processes. The same on macOS, Linux and Windows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The commands that settle a copy from its Records: the explicit close and prune.
const SETTLES: &[&str] = &["close-session", "prune-sessions"];
/// The commands that run Repair before their own work, which leaves a copy as it is.
const REPAIRS: &[&str] = &["sessions", "tell"];
/// The commands that answer for a copy as for any session.
const OTHERS: &[&str] = &[
    "hold", "cancel", "reopen", "status", "inspect", "result", "wait", "search", "screen", "doctor",
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
    /// The Owner record that the terminal's own OS writes, bound.
    Full,
    /// A bound PID-only record.
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
}

fn handle(terminal: &str) -> Value {
    json!({"terminal": terminal, "session_id": "surface", "managed_session_id": ID})
}

// A Herdr handle born on `origin`, naming a socket in the fixture root and its host's PID.
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

// The Owner record that `os`'s writer records, bound to the session `id`.
fn owner(os: &str, id: &str, pid: u32) -> Value {
    let mut owner = json!({"pid": pid, "managed_session_id": id});
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

    // Stops listening and removes the endpoints, so that a connection finds no core.
    fn close(&mut self) {
        for listener in self.0.drain(..) {
            if let Ok(address) = listener.local_addr()
                && let Some(path) = address.as_pathname()
            {
                let _ = fs::remove_file(path);
            }
        }
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

    fn close(&mut self) {}
}

struct Fixture {
    root: tempfile::TempDir,
    endpoints: Endpoints,
    /// Every PID the fixture records.
    pid: u32,
}

// One session directory of the fixture root, with its manifest, status and lifecycle locks.
fn session(root: &Path, id: &str, state: &str, error: Option<&str>) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(directory.join("events")).unwrap();
    fs::create_dir_all(directory.join("requests")).unwrap();
    // The lifecycle locks exist, as in every session, so that taking one writes nothing.
    for lock in ["turn.claim.lock", "status.lock"] {
        fs::write(directory.join(lock), b"").unwrap();
    }
    support::write(
        &directory.join("manifest.json"),
        &support::manifest(
            id,
            "pi",
            "pi".as_ref(),
            "1.5.0",
            &root.canonicalize().unwrap(),
            "copied from another OS",
        ),
    );
    support::write(
        &directory.join("status.json"),
        &support::status(state, 2, error),
    );
    directory
}

// The provider process a launch wrapper records, for `pid`.
fn provider_process(directory: &Path, id: &str, pid: u32) {
    support::write(
        &directory.join("provider-process.json"),
        &json!({"schema": 1, "managed_session_id": id, "pid": pid, "spawned_unix_ms": 1}),
    );
}

/// The recorded fields that hold a process id: what differs between two runs of one case.
const PID_FIELDS: &[&str] = &["pid", "host_pid", "process_group", "terminal_process_group"];

impl Fixture {
    // The fixture of `case` with every recorded PID `pid`.
    fn new(case: &Case, pid: u32) -> Self {
        let root = tempfile::tempdir().unwrap();
        let error = (case.state == "failed").then_some(FAILURE);
        let directory = session(root.path(), ID, case.state, error);
        provider_process(&directory, ID, pid);
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
            terminal["herdr_core"]["host_pid"] = json!(pid);
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
                &owner(case.origin, ID, pid),
            ),
            Owner::PidOnly => support::write(
                &directory.join("native-session.json"),
                &json!({"pid": pid, "managed_session_id": ID}),
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
        Self {
            root,
            endpoints,
            pid,
        }
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

    // A text as two runs of one case must share it: the root as the commands name it, the
    // recorded PID only where a known diagnostic names it, and the time a launch log line
    // starts with. Every other number, a boot id's included, is kept as it is.
    fn normalized_text(&self, text: &str) -> String {
        let mut text = text.to_owned();
        let canonical = self.root.path().canonicalize().unwrap();
        for root in [canonical.as_path(), self.root.path()] {
            text = text.replace(&root.display().to_string(), "<root>");
        }
        // Repair's diagnosis of an owner it found gone names the owner's PID.
        text.replace(
            &format!("native session process {} ", self.pid),
            "native session process <pid> ",
        )
    }

    // A JSON value as two runs of one case must share it: its texts normalized, a recorded
    // process id (`PID_FIELDS`) equal to the fixture's replaced, and the times a command
    // records when it runs (`*_ms`) zeroed.
    fn normalized_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.normalized_text(text)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.normalized_value(item))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, child)| {
                        let child = if PID_FIELDS.contains(&key.as_str())
                            && child.as_u64() == Some(u64::from(self.pid))
                        {
                            json!("<pid>")
                        } else if key.ends_with("_ms") && child.is_number() {
                            json!(0)
                        } else {
                            self.normalized_value(child)
                        };
                        (key.clone(), child)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    // A command's output as two runs must share it.
    fn answer(&self, output: &Output) -> (Option<i32>, Value, String) {
        let stdout = match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(value) => self.normalized_value(&value),
            Err(_) => Value::String(self.normalized_text(&String::from_utf8_lossy(&output.stdout))),
        };
        (
            output.status.code(),
            stdout,
            self.normalized_text(&String::from_utf8_lossy(&output.stderr)),
        )
    }

    // The records as two runs must share them: relative paths, and their contents normalized.
    // A launch log line starts with the time it was written.
    fn shared_records(&self) -> Vec<(String, Option<Value>)> {
        self.records()
            .into_iter()
            .map(|(path, bytes)| {
                let relative = path
                    .strip_prefix(self.root.path())
                    .unwrap()
                    .display()
                    .to_string();
                let log = path.ends_with("launch.log");
                let content = bytes.map(|bytes| match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => self.normalized_value(&value),
                    Err(_) => {
                        let text = String::from_utf8_lossy(&bytes);
                        let text = if log {
                            text.lines()
                                .map(|line| line.split_once(' ').map_or(line, |(_, rest)| rest))
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            text.into_owned()
                        };
                        Value::String(self.normalized_text(&text))
                    }
                });
                (relative, content)
            })
            .collect()
    }
}

// A record of a snapshot as JSON; null when it is missing or not JSON.
fn recorded(entries: &[support::Entry], path: &Path) -> Value {
    entries
        .iter()
        .find(|(entry, _)| entry == path)
        .and_then(|(_, bytes)| bytes.as_ref())
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .unwrap_or(Value::Null)
}

// The entries two snapshots do not share.
fn differing<'a>(
    one: &'a [(String, Option<Value>)],
    other: &'a [(String, Option<Value>)],
) -> Vec<&'a (String, Option<Value>)> {
    one.iter()
        .filter(|entry| !other.contains(entry))
        .chain(other.iter().filter(|entry| !one.contains(entry)))
        .collect()
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
    let args: Vec<&str> = match command {
        "tell" => vec!["tell", ID, "--prompt", "hello", "--json"],
        "close-session" => vec!["close-session", ID, "--explicit", "--json"],
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

// The row a listing gives this fixture's session.
fn row<'a>(rows: &'a Value, id: &str) -> &'a Value {
    rows.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id))
        .unwrap_or(&Value::Null)
}

// What a command answers for this fixture: `Some(Err(text))` a failure whose error is `text`
// or ends with it, `Some(Ok(details))` a success with those fields, `None` an answer this test
// does not pin (`tell`'s own, after its Repair, is the same as for any session).
fn expected(command: &str, case: &Case, fixture: &Fixture) -> Option<Result<Value, String>> {
    Some(match command {
        "close-session" if !case.settles() => Err(fixture.refusal(&case.head)),
        "close-session" => Ok(json!({"closed": true})),
        "prune-sessions" if case.closed => Ok(json!({"pruned": [ID]})),
        "prune-sessions" => Ok(json!({"pruned": []})),
        "sessions" => Ok(json!({})),
        _ => return None,
    })
}

// The state `sessions` lists for the fixture's session: its records' state once Repair has
// run (an expired launch fails from its records, as before).
fn listed_state(case: &Case) -> &'static str {
    if case.closed {
        "closed"
    } else if case.expired_launch {
        "failed"
    } else {
        case.state
    }
}

#[test]
fn every_public_command_is_classified_for_a_session_of_another_os() {
    let probe = Fixture::new(&cases()[0], std::process::id());
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
        let classes = [SETTLES, REPAIRS, OTHERS, NO_SESSION]
            .iter()
            .filter(|class| class.contains(&name.as_str()))
            .count();
        assert_eq!(
            classes, 1,
            "{name} must be on exactly one list: does it settle a copy, run Repair, answer as for any session, or take no session?"
        );
    }
    for class in [SETTLES, REPAIRS, OTHERS, NO_SESSION] {
        for name in class {
            assert!(
                names.iter().any(|listed| listed == name),
                "{name} is not public"
            );
        }
    }
}

// The normalizer replaces only a recorded process id and the diagnostics known to name one:
// with the fixture's PID equal to a component of the Linux boot id the fixtures record
// (6677), the boot id and every other number stay as they are.
#[test]
fn the_normalizer_keeps_every_number_that_is_not_a_recorded_pid() {
    let fixture = Fixture::new(&cases()[0], 6677);
    let value = json!({
        "linux_birth": {"boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 6677},
        "pid": 6677, "host_pid": 6677, "count": 6677, "process_group": 6677,
        "error": "native session process 6677 is no longer running; 6677 requests",
        "updated_unix_ms": 1795,
    });
    assert_eq!(
        fixture.normalized_value(&value),
        json!({
            "linux_birth": {"boot_id": "00112233-4455-6677-8899-aabbccddeeff",
                "starttime_ticks": 6677},
            "pid": "<pid>", "host_pid": "<pid>", "count": 6677, "process_group": "<pid>",
            "error": "native session process <pid> is no longer running; 6677 requests",
            "updated_unix_ms": 0,
        })
    );
}

// The explicit close, prune and the commands that run Repair, against every fixture, twice:
// with every recorded PID alive here, then with every one absent. Both runs must give the same
// answer and leave the same records, and the first run's answer must be the expected one.
#[test]
fn the_close_prune_and_repair_answer_a_copy_from_its_records_alone() {
    // Every departure is collected, so that one run names all of them.
    let mut failures = Vec::new();
    let alive = std::process::id();
    for case in cases() {
        for command in SETTLES.iter().chain(REPAIRS) {
            let runs: Vec<_> = [alive, absent_pid()]
                .into_iter()
                .map(|pid| {
                    let mut fixture = Fixture::new(&case, pid);
                    // `tell` goes on, after its Repair, as for any session: its ownership proof
                    // addresses the core a Herdr handle names, which a listener that never
                    // answers would hold. Without one the core is simply absent.
                    if *command == "tell" {
                        fixture.endpoints.close();
                    }
                    let before = fixture.records();
                    let output = fixture.run(command);
                    let after = fixture.records();
                    // A settled copy, once its retention has passed, is pruned too.
                    let pruned = (*command == "close-session" && case.settles()).then(|| {
                        let settled = fixture.shared_records();
                        fixture.age_closed_records();
                        (settled, fixture.run("prune-sessions"))
                    });
                    (fixture, before, output, after, pruned)
                })
                .collect();
            let [
                (fixture, before, output, after, pruned),
                (other, _, other_output, _, other_pruned),
            ] = <[_; 2]>::try_from(runs).ok().unwrap();
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
            // The same answer and the same records whether the recorded PIDs are alive here.
            check(
                fixture.answer(&output) == other.answer(&other_output),
                &format!(
                    "the answer depends on whether its recorded PIDs are alive here; with them absent: {}{}",
                    String::from_utf8_lossy(&other_output.stdout),
                    String::from_utf8_lossy(&other_output.stderr)
                ),
            );
            let (records, other_records) = (fixture.shared_records(), other.shared_records());
            check(
                records == other_records,
                &format!(
                    "the records depend on whether its recorded PIDs are alive here: {:?}",
                    differing(&records, &other_records)
                ),
            );
            if let (Some((settled, pruned)), Some((other_settled, other_pruned))) =
                (&pruned, &other_pruned)
            {
                check(
                    settled == other_settled
                        && fixture.answer(pruned) == other.answer(other_pruned),
                    "the settlement or its prune depends on whether its recorded PIDs are alive here",
                );
            }
            // `tell` goes on, after its Repair, as for any session, which addresses the
            // terminal its handle names; the close, prune and Repair address none.
            if *command != "tell" {
                for run in [&fixture, &other] {
                    check(
                        run.endpoints.connections() == 0,
                        "connected to the terminal's endpoint",
                    );
                }
            }
            check(
                !text.contains("relative-herdr-core.sock"),
                "addressed a Herdr Core of this machine",
            );
            let value = reply(&output);
            match expected(command, &case, &fixture) {
                Some(Err(answer)) => {
                    check(!output.status.success(), "succeeded");
                    let error = failure(&output);
                    check(
                        error == answer || error.ends_with(&answer),
                        &format!("not `{answer}`"),
                    );
                }
                Some(Ok(Value::Object(details))) => {
                    check(output.status.success(), "failed");
                    check(!value.is_null(), "no JSON answer");
                    for (key, expected) in details {
                        check(
                            value[&key] == expected,
                            &format!("`{key}` is not {expected}"),
                        );
                    }
                }
                _ => {}
            }
            if *command == "sessions" {
                let listed = row(&value, ID);
                check(
                    listed["state"] == listed_state(&case),
                    &format!("its sessions row is not {}", listed_state(&case)),
                );
            }
            // What may change: an expired launch is settled from its records by the commands
            // that run Repair, as before; the close and prune settle.
            let directory = fixture.directory();
            let record = |name: &str| recorded(&after, &directory.join(name));
            match *command {
                "close-session" if case.settles() => {
                    let status = record("status.json");
                    check(status["state"] == "closed", "status is not closed");
                    check(
                        record("closed.json")["state"] == "closed",
                        "no closed record",
                    );
                    check(
                        record("terminal.json").is_null()
                            && record("terminal.closing.json").is_null(),
                        "a handle remains",
                    );
                    let marker = record("terminal.closed.json");
                    check(
                        marker["consumed"] == true && marker["terminal"] == case.kind,
                        "the consumption marker does not keep the consumed kind",
                    );
                    let error = status["error"].as_str().unwrap_or_default().to_owned();
                    if !case.closed {
                        check(
                            error.contains("settled from its records alone"),
                            "the closed status does not say how it was settled",
                        );
                    }
                    if case.state == "failed" {
                        check(error.contains(FAILURE), "the failure diagnosis was lost");
                    }
                    // The same copy, once its retention has passed, is pruned without a local
                    // observation of its owner.
                    let (_, pruned) = pruned.as_ref().unwrap();
                    check(
                        reply(pruned)["pruned"] == json!([ID]) && !directory.exists(),
                        "the settled copy was not pruned",
                    );
                }
                "prune-sessions" if case.closed => {
                    check(!directory.exists(), "the closed copy remains");
                }
                "tell" | "sessions" if case.expired_launch => {
                    // The expired launch failed from its records, its claim released and the
                    // failure logged, and nothing else changed.
                    let status = record("status.json");
                    check(
                        status["state"] == "failed",
                        "the expired launch did not fail",
                    );
                    check(
                        status["error"]
                            .as_str()
                            .is_some_and(|error| error.contains("timed out")),
                        "the expired launch does not say it timed out",
                    );
                    check(
                        !after.iter().any(|(path, _)| path.ends_with("turn.claim")),
                        "the expired launch kept its claim",
                    );
                    let others = |entries: Vec<support::Entry>| -> Vec<support::Entry> {
                        entries
                            .into_iter()
                            .filter(|(path, _)| {
                                !path.ends_with("status.json")
                                    && !path.ends_with("turn.claim")
                                    && !path.ends_with("launch.log")
                            })
                            .collect()
                    };
                    check(
                        others(after) == others(before),
                        "the expired launch changed another record",
                    );
                }
                _ => check(after == before, "changed the records"),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// The close answers the same records in one order: a handle that is not this session's
// before the session of another OS, with a legacy iTerm2 handle that names no session counting
// as its own; and a closed copy's close finishes again from its records, keeping the kind its
// remaining handle states.
#[test]
fn the_close_answers_a_copys_records_in_one_order() {
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
    // answers before the OS, and nothing is written.
    let mut unbound = ready.clone();
    unbound.terminal["managed_session_id"] = json!("session-other");
    let binding = format!("terminal handle belongs to managed session session-other, not {ID}");
    let fixture = Fixture::new(&unbound, absent_pid());
    let before = fixture.records();
    let output = fixture.run("close-session");
    assert!(!output.status.success());
    let error = failure(&output);
    assert!(error.ends_with(&binding), "{error}");
    assert_eq!(fixture.records(), before);

    // A legacy iTerm2 handle without a binding, beside an Owner record of macOS: on an OS
    // that does not run iTerm2 the OS the owner names answers.
    if os != "macOS" {
        let legacy = Case {
            terminal: json!({"terminal": "iterm2", "session_id": "surface"}),
            origin: "macOS",
            ..ready.clone()
        };
        let fixture = Fixture::new(&legacy, absent_pid());
        let refusal = fixture.refusal(&format!(
            "a session recorded on macOS is not available on {os}"
        ));
        let error = failure(&fixture.run("close-session"));
        assert!(error.ends_with(&refusal), "{error}");
    }

    // A closed session whose bound handle of another OS remains: its records alone finish the
    // close, as for any closed session: the handle is consumed, its kind kept, and no terminal
    // is reached.
    let stale = Case {
        state: "closed",
        ..ready
    };
    let fixture = Fixture::new(&stale, absent_pid());
    support::write(
        &fixture.directory().join("closed.json"),
        &support::status("closed", 2, None),
    );
    let output = fixture.run("close-session");
    assert!(output.status.success(), "{}", failure(&output));
    assert!(!fixture.directory().join("terminal.json").exists());
    assert_eq!(
        read_json(&fixture.directory().join("terminal.closed.json"))["terminal"],
        terminal
    );
    assert_eq!(fixture.endpoints.connections(), 0);
}
