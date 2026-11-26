//! A Session whose Records belong to another OS, as a state directory copied between machines
//! holds it. Its Records say so positively: the kind of its active, claimed or consumed handle,
//! a Herdr handle's recorded host birth, or the identity its Owner record carries. A PID-only or
//! absent Owner record does not hide such a terminal.
//!
//! Every public command that `describe` lists is classified below, and the test runs each one
//! against every fixture. A command that acts on a session gives the one refusal (or, for a
//! closed copy, the closed answer that comes first) and writes nothing. The explicit close and
//! prune decide from the Records alone: the close settles a copy they show ended (exited or
//! failed) with no terminal call and refuses one they leave running; prune removes a closed copy
//! and keeps the others. A command that only reads keeps working, writes nothing and reports no
//! observation of this machine's processes; a command that takes no session is listed as such.
//! A command added later that is on none of the lists fails the test. The same on macOS, Linux
//! and Windows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

/// Commands that act on a session and never settle a copy: each gives the one refusal and
/// writes nothing.
const ACTS: &[&str] = &["tell", "hold", "cancel", "reopen", "screen"];
/// Commands that settle a copy its Records show ended, from those Records alone.
const SETTLES: &[&str] = &["close-session", "prune-sessions"];
/// Commands that only read records (`sessions` runs Repair, which leaves such a session as it
/// is): each writes nothing and reports no observation of a local process about the owner.
const READS: &[&str] = &[
    "status", "inspect", "result", "wait", "search", "doctor", "sessions",
];
/// Commands that take no session.
const NO_SESSION: &[&str] = &["describe", "ask", "self-test", "consent", "settings"];

const ID: &str = "session-foreign";
const CLAIM: &str = "7-7-7";
const REQUEST: &str = "request-foreign";

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
    /// The Owner record that the terminal's own OS writes, for a PID absent here.
    Full,
    /// A PID-only record whose PID is this test's own, alive here: any local observation of
    /// it would report a live process.
    PidOnly,
    Absent,
}

/// Where the fixture's handle is recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Handle {
    /// The active handle (`terminal.json`).
    Active,
    /// Claimed by a close that was interrupted before it consumed it (`terminal.closing.json`).
    Claimed,
    /// Consumed by a close: its kind is kept in the consumption marker only
    /// (`terminal.closed.json`).
    Consumed,
}

#[derive(Clone, Debug)]
struct Case {
    name: String,
    /// The handle record, or the consumption marker for a consumed one.
    terminal: Value,
    kind: &'static str,
    handle: Handle,
    origin: &'static str,
    owner: Owner,
    state: &'static str,
    /// The session is closed (`closed.json`).
    closed: bool,
    /// A launch whose receipt deadline has passed, with its claim.
    expired_launch: bool,
    /// A turn in progress: a claim and its request.
    working: bool,
    /// What the Records name: the start of the one refusal.
    head: String,
}

impl Case {
    // Whether the explicit close settles it from its records: they show it ended, or a close
    // already consumed its handle, or it is closed.
    fn settles(&self) -> bool {
        self.closed || self.handle == Handle::Consumed || ["exited", "failed"].contains(&self.state)
    }
}

fn handle(terminal: &str) -> Value {
    json!({"terminal": terminal, "session_id": "surface", "managed_session_id": ID})
}

fn herdr(origin: &str) -> Value {
    let birth = match origin {
        "macOS" => json!({"host_start_seconds": 1, "host_start_microseconds": 2}),
        "Linux" => json!({"linux_birth": {
            "boot_id": "00112233-4455-6677-8899-aabbccddeeff", "starttime_ticks": 7}}),
        _ => json!({"host_creation_time": 1}),
    };
    let mut core = json!({"socket": "/tmp/herdr-core.sock", "core_id": "core-a",
        "created_ms": 1, "host_pid": 4});
    for (key, value) in birth.as_object().unwrap() {
        core[key] = value.clone();
    }
    json!({"terminal": "herdr", "session_id": "t7", "managed_session_id": ID, "herdr_core": core})
}

// Every fixture of a session of another OS this host can be given.
fn cases() -> Vec<Case> {
    let os = here();
    let mut cases = Vec::new();
    let base = |name: String, terminal, kind, origin, owner, head: String| Case {
        name,
        terminal,
        kind,
        handle: Handle::Active,
        origin,
        owner,
        state: "ready",
        closed: false,
        expired_launch: false,
        working: false,
        head,
    };
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
        let head = format!("{name} is not available on {os}");
        let base = |name: String, owner| {
            base(
                name,
                handle(terminal),
                terminal,
                origin,
                owner,
                head.clone(),
            )
        };
        for owner in [Owner::Full, Owner::PidOnly, Owner::Absent] {
            cases.push(base(format!("{terminal}, owner {owner:?}"), owner));
        }
        cases.push(Case {
            state: "launching",
            expired_launch: true,
            ..base(
                format!("{terminal}, expired launch, no owner"),
                Owner::Absent,
            )
        });
        cases.push(Case {
            state: "working",
            working: true,
            ..base(format!("{terminal}, working"), Owner::Full)
        });
        cases.push(Case {
            state: "working",
            handle: Handle::Claimed,
            ..base(
                format!("{terminal}, working, interrupted close"),
                Owner::Full,
            )
        });
        cases.push(Case {
            state: "exited",
            ..base(format!("{terminal}, exited"), Owner::Full)
        });
        cases.push(Case {
            state: "failed",
            ..base(
                format!("{terminal}, failed, PID-only owner"),
                Owner::PidOnly,
            )
        });
        cases.push(Case {
            state: "failed",
            handle: Handle::Claimed,
            ..base(
                format!("{terminal}, failed, interrupted close"),
                Owner::Full,
            )
        });
        cases.push(Case {
            state: "exited",
            handle: Handle::Consumed,
            terminal: json!({"consumed": true, "terminal": terminal}),
            ..base(format!("{terminal}, exited, handle consumed"), Owner::Full)
        });
        cases.push(Case {
            state: "closed",
            closed: true,
            handle: Handle::Consumed,
            terminal: json!({"consumed": true, "terminal": terminal}),
            ..base(format!("{terminal}, closed copy"), Owner::Full)
        });
    }
    for origin in ["macOS", "Linux", "Windows"] {
        if origin != os {
            let born = base(
                format!("herdr born on {origin}, no owner"),
                herdr(origin),
                "herdr",
                origin,
                Owner::Absent,
                format!("a session recorded on {origin} is not available on {os}"),
            );
            cases.push(Case {
                state: "exited",
                name: format!("herdr born on {origin}, exited, no owner"),
                ..born.clone()
            });
            cases.push(born);
        }
    }
    cases
}

// The Owner record that `os`'s writer records.
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

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new(case: &Case) -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let directory = fixture.directory();
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
                &fixture.root.path().canonicalize().unwrap(),
                "copied from another OS",
            ),
        );
        support::write(
            &directory.join("status.json"),
            &support::status(case.state, 2, None),
        );
        if case.closed {
            support::write(
                &directory.join("closed.json"),
                &support::status("closed", 2, None),
            );
        }
        let record = match case.handle {
            Handle::Active => "terminal.json",
            Handle::Claimed => "terminal.closing.json",
            Handle::Consumed => "terminal.closed.json",
        };
        support::write(&directory.join(record), &case.terminal);
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
        fixture
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
}

fn reply(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
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

// What a command that acts answers for this fixture: the closed answer for a closed copy (the
// session's own state comes first), a surface being closed or already consumed for `screen`,
// the one refusal otherwise. `None` where the answer is the command's own read of a missing
// active handle (`tell` reads `terminal.json`).
fn acting_answer(command: &str, case: &Case, refusal: &str) -> Option<String> {
    if case.closed && command != "reopen" {
        return Some(match command {
            "tell" => format!("session {ID} is closed; tell requires the ready state"),
            "hold" => "the session is closed; a hold cannot be set or released".to_owned(),
            "cancel" => {
                format!("session {ID} is closed; cancel requires the running or working state")
            }
            _ => format!("session {ID} is closed; its terminal surface is not read"),
        });
    }
    if case.handle != Handle::Active {
        match command {
            "screen" => {
                return Some(format!(
                    "the terminal surface of session {ID} is closed or being closed"
                ));
            }
            "tell" => return None,
            _ => {}
        }
    }
    Some(refusal.to_owned())
}

// Every owner observation a read reports for the session: none may come from this machine.
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

// A record as JSON; null when it is missing or unreadable, so that a departure is reported.
fn read_json(path: &std::path::Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

#[test]
fn every_public_command_is_classified_for_a_session_of_another_os() {
    let probe = Fixture::new(&cases()[0]);
    let described = reply(&probe.output(&["describe", "--json"]));
    let names: Vec<String> = described["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!names.is_empty());
    for name in &names {
        let classes = [ACTS, SETTLES, READS, NO_SESSION]
            .iter()
            .filter(|class| class.contains(&name.as_str()))
            .count();
        assert_eq!(
            classes, 1,
            "{name} must be on exactly one list: does it act on a session, settle one from its records, only read records, or take no session?"
        );
    }
    for class in [ACTS, SETTLES, READS, NO_SESSION] {
        for name in class {
            assert!(
                names.iter().any(|listed| listed == name),
                "{name} is not public"
            );
        }
    }
}

#[test]
fn a_session_of_another_os_is_refused_settled_from_its_records_or_only_read() {
    // Every departure is collected, so that one run names all of them.
    let mut failures = Vec::new();
    for case in cases() {
        for command in ACTS.iter().chain(SETTLES).chain(READS) {
            let fixture = Fixture::new(&case);
            let refusal = fixture.refusal(&case.head);
            let before = support::entries(fixture.root.path());
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
                !text.contains("relative-herdr-core.sock"),
                "addressed a Herdr Core",
            );
            let unchanged = support::entries(fixture.root.path()) == before;
            if ACTS.contains(command) {
                check(!output.status.success(), "succeeded");
                check(unchanged, "changed the records");
                let Some(answer) = acting_answer(command, &case, &refusal) else {
                    continue;
                };
                let error = failure(&output);
                check(
                    error == answer || error.ends_with(&format!(": {answer}")),
                    &format!("not `{answer}`"),
                );
                if answer == refusal {
                    check(
                        reply(&output)["failure_kind"] == "unsupported",
                        "not an unsupported failure",
                    );
                }
                continue;
            }
            if *command == "close-session" {
                let directory = fixture.directory();
                if !case.settles() {
                    // A copy its records leave running is not ended from here.
                    check(!output.status.success(), "succeeded");
                    check(unchanged, "changed the records");
                    check(
                        String::from_utf8_lossy(&output.stderr)
                            .ends_with(&format!("Caused by:\n    {refusal}\n")),
                        &format!("not `{refusal}`"),
                    );
                    continue;
                }
                // Settled from the records alone: the handle, active or claimed, is consumed
                // as a close consumes it, keeping its kind, and the session is closed.
                check(output.status.success(), "did not settle");
                check(
                    read_json(&directory.join("status.json"))["state"] == "closed",
                    "status is not closed",
                );
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
                    "the consumption marker does not keep the kind",
                );
                if !case.closed && case.handle != Handle::Consumed {
                    check(
                        read_json(&directory.join("status.json"))["error"]
                            .as_str()
                            .is_some_and(|error| {
                                error.contains(&format!(
                                    "settled from its records alone ({})",
                                    case.head
                                ))
                            }),
                        "the closed status does not say how it was settled",
                    );
                }
                continue;
            }
            if *command == "prune-sessions" {
                let pruned = reply(&output)["pruned"].clone();
                check(output.status.success(), "failed");
                if case.closed {
                    // A closed copy is removed, whatever OS recorded its owner.
                    check(pruned == json!([ID]), "the closed copy was kept");
                    check(!fixture.directory().exists(), "the directory remains");
                } else {
                    check(pruned == json!([]), "an open copy was pruned");
                    check(unchanged, "changed the records");
                }
                continue;
            }
            check(unchanged, "changed the records");
            let value = reply(&output);
            for observation in owner_observations(&value) {
                check(observation.is_null(), "reported a local owner observation");
            }
            if *command == "doctor" {
                // The probe's answer: the closed one first, then a missing active handle, the
                // one refusal otherwise.
                let answer = if case.closed {
                    format!("session {ID} is closed; its terminal surface is not read")
                } else if case.handle != Handle::Active {
                    "no active terminal handle is recorded".to_owned()
                } else {
                    refusal.clone()
                };
                check(
                    doctor_check(&value, "terminal_surface")["detail"]
                        == format!("Surface presence could not be verified: {answer}"),
                    "the probe did not give the one answer",
                );
            }
            if *command == "wait" && case.working {
                check(value["timed_out"] == true, "the wait ended");
            }
            if *command == "inspect" {
                // The capabilities project what `tell` and `cancel` would answer.
                let refused = json!({"state": "refused", "reason": "surface_unsupported",
                    "detail": refusal});
                let (tell, cancel) = if case.closed {
                    (
                        json!({"state": "refused", "reason": "state_not_ready",
                            "detail": format!("session {ID} is closed; tell requires the ready state")}),
                        json!({"state": "refused", "reason": "state_not_running",
                            "detail": format!("session {ID} is closed; cancel requires the running or working state")}),
                    )
                } else if case.handle != Handle::Active {
                    (
                        json!({"state": "refused", "reason": "surface_closed",
                            "detail": "the terminal surface is closed or being closed"}),
                        refused,
                    )
                } else {
                    (refused.clone(), refused)
                };
                check(value["capabilities"]["tell"] == tell, "the tell capability");
                check(
                    value["capabilities"]["cancel"] == cancel,
                    "the cancel capability",
                );
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// The same records get the same answer from every command, in one order: a closed session
// first, then a handle that is not this session's, then a session of another OS. A legacy
// iTerm2 handle without a binding still counts as this session's.
#[test]
fn every_command_answers_the_same_records_in_one_order() {
    let os = here();
    let (terminal, origin) = if os == "Windows" {
        ("apple-terminal", "macOS")
    } else {
        ("windows-console", "Windows")
    };
    let mut ready = cases()
        .into_iter()
        .find(|case| {
            case.terminal["terminal"] == terminal
                && case.owner == Owner::Full
                && case.state == "ready"
        })
        .unwrap();
    assert_eq!(ready.origin, origin);

    // A handle bound to another session: the binding answers before the OS.
    ready.terminal["managed_session_id"] = json!("session-other");
    let binding = format!("terminal handle belongs to managed session session-other, not {ID}");
    let fixture = Fixture::new(&ready);
    let before = support::entries(fixture.root.path());
    for command in ["tell", "screen", "close-session"] {
        let output = fixture.run(command);
        assert!(!output.status.success(), "{command}");
        let error = failure(&output);
        assert!(
            error == binding || error.ends_with(&format!(": {binding}")),
            "{command}: {error}"
        );
    }
    let inspected = reply(&fixture.output(&["inspect", ID, "--json"]));
    assert_eq!(
        inspected["capabilities"]["tell"]["reason"], "surface_unbound",
        "{inspected}"
    );
    let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
    assert_eq!(
        doctor_check(&doctor, "terminal_record")["reason_code"],
        "terminal_binding_unverified"
    );
    assert_eq!(
        doctor_check(&doctor, "terminal_surface")["detail"],
        format!("Surface presence could not be verified: {binding}")
    );
    assert_eq!(support::entries(fixture.root.path()), before);

    // A legacy iTerm2 handle without a binding is this session's: on an OS that does not run
    // iTerm2 the OS answers.
    if os != "macOS" {
        let legacy = Case {
            terminal: json!({"terminal": "iterm2", "session_id": "surface"}),
            origin: "macOS",
            ..ready.clone()
        };
        let fixture = Fixture::new(&legacy);
        let refusal = fixture.refusal(&format!("iTerm2 is not available on {os}"));
        for command in ["tell", "screen", "close-session"] {
            let error = failure(&fixture.run(command));
            assert!(
                error == refusal || error.ends_with(&format!(": {refusal}")),
                "{command}: {error}"
            );
        }
        let inspected = reply(&fixture.output(&["inspect", ID, "--json"]));
        assert_eq!(
            inspected["capabilities"]["tell"],
            json!({"state": "refused", "reason": "surface_unsupported", "detail": refusal})
        );
        let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
        assert_eq!(
            doctor_check(&doctor, "terminal_record")["reason_code"],
            "terminal_unsupported"
        );
        assert_eq!(
            doctor_check(&doctor, "terminal_surface")["detail"],
            format!("Surface presence could not be verified: {refusal}")
        );
    }

    // A closed session whose bound handle of another OS remains: closed answers first.
    let stale = Case {
        state: "closed",
        ..cases()
            .into_iter()
            .find(|case| {
                case.terminal["terminal"] == terminal
                    && case.owner == Owner::Full
                    && case.state == "ready"
            })
            .unwrap()
    };
    let fixture = Fixture::new(&stale);
    support::write(
        &fixture.directory().join("closed.json"),
        &support::status("closed", 2, None),
    );
    let before = support::entries(fixture.root.path());
    for (command, answer) in [
        (
            "tell",
            format!("session {ID} is closed; tell requires the ready state"),
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
            "detail": format!("session {ID} is closed; tell requires the ready state")})
    );
    let doctor = reply(&fixture.output(&["doctor", ID, "--probe", "--json"]));
    assert_eq!(
        doctor_check(&doctor, "terminal_record")["reason_code"],
        "terminal_session_closed"
    );
    assert_eq!(
        doctor_check(&doctor, "terminal_surface")["detail"],
        format!(
            "Surface presence could not be verified: session {ID} is closed; its terminal surface is not read"
        )
    );
    assert_eq!(support::entries(fixture.root.path()), before);
    // Its records alone settle the close, as for any closed session: the handle is consumed,
    // its kind kept, and no terminal is reached.
    let output = fixture.run("close-session");
    assert!(output.status.success(), "{}", failure(&output));
    assert!(!fixture.directory().join("terminal.json").exists());
    assert_eq!(
        read_json(&fixture.directory().join("terminal.closed.json"))["terminal"],
        terminal
    );
}
