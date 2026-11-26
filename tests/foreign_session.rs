//! A Session whose Records belong to another OS, as a state directory copied between machines
//! holds it: a terminal this OS does not run, or a Herdr terminal, with the Owner record that its
//! own OS writes and an owner PID that is absent here. Through the public commands, everything
//! that would act on it gives the one refusal and changes nothing, Repair does not settle it
//! from this machine's processes, and the read-only commands keep working, the same on macOS,
//! Linux and Windows.
#![cfg(any(target_os = "macos", target_os = "linux", windows))]
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

const ID: &str = "session-foreign";

// This OS's name and the terminals it runs, as the refusal names them.
fn here() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        (
            "macOS",
            "iterm2, apple-terminal, ghostty, warp, wezterm or herdr",
        )
    } else if cfg!(target_os = "linux") {
        ("Linux", "tmux or herdr")
    } else {
        ("Windows", "windows-console or herdr")
    }
}

// (terminal record name, the OS that writes it, the refusal): each terminal this OS does not
// run with its own OS's owner, and Herdr with each other OS's owner.
fn cases() -> Vec<(&'static str, &'static str, String)> {
    let (os, alternatives) = here();
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
        if origin != os {
            cases.push((
                terminal,
                origin,
                format!("{name} is not available on {os}; use {alternatives}"),
            ));
        }
    }
    for origin in ["macOS", "Linux", "Windows"] {
        if origin != os {
            cases.push((
                "herdr",
                origin,
                format!("a session recorded on {origin} is not available on {os}"),
            ));
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
    fn new(terminal: &str, origin: &str, state: &str) -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let directory = fixture.directory();
        fs::create_dir_all(directory.join("events")).unwrap();
        support::write(
            &directory.join("manifest.json"),
            &support::manifest(
                ID,
                "claude",
                "claude".as_ref(),
                "2.1.302",
                &fixture.root.path().canonicalize().unwrap(),
                "copied from another OS",
            ),
        );
        support::write(
            &directory.join("status.json"),
            &support::status(state, 2, None),
        );
        support::write(
            &directory.join("terminal.json"),
            &json!({"terminal": terminal, "session_id": "surface", "managed_session_id": ID}),
        );
        support::write(
            &directory.join("native-session.json"),
            &owner(origin, absent_pid()),
        );
        fixture
    }

    fn directory(&self) -> PathBuf {
        self.root.path().join(ID)
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?}: {error}: {output:?}"))
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            // No Herdr Core of this machine is ever addressed.
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HC_SESSION")
            .env("HC_SOCKET", "relative-herdr-core.sock")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn handle(&self) -> Option<Vec<u8>> {
        fs::read(self.directory().join("terminal.json")).ok()
    }
}

#[test]
fn every_command_that_would_act_on_a_session_of_another_os_gives_one_refusal() {
    for (terminal, origin, refusal) in cases() {
        for state in ["ready", "failed"] {
            let case = format!("{terminal} recorded on {origin}, {state}");
            let fixture = Fixture::new(terminal, origin, state);
            let handle = fixture.handle();
            let unchanged = |command: &str| {
                assert_eq!(fixture.handle(), handle, "{case}, {command}");
                assert!(!fixture.directory().join("closed.json").exists(), "{case}");
                assert!(!fixture.directory().join("turn.claim").exists(), "{case}");
            };
            let refused = |reply: Value, command: &str| {
                assert_eq!(reply["ok"], false, "{case}, {command}: {reply}");
                assert_eq!(
                    reply["error"],
                    refusal.as_str(),
                    "{case}, {command}: {reply}"
                );
                assert_eq!(
                    reply["failure_kind"], "unsupported",
                    "{case}, {command}: {reply}"
                );
                unchanged(command);
            };
            refused(
                fixture.run(&["tell", ID, "--prompt", "hello", "--json"]),
                "tell",
            );
            // `close-session` reports a failure on stderr, under its own context line.
            for attempt in ["close", "close again"] {
                let output = fixture.output(&["close-session", ID, "--explicit", "--json"]);
                assert!(!output.status.success(), "{case}, {attempt}: {output:?}");
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.ends_with(&format!("Caused by:\n    {refusal}\n")),
                    "{case}, {attempt}: {stderr}"
                );
                unchanged(attempt);
            }
            // `screen` refuses a kind this OS does not run from its records, and a Herdr terminal
            // of another OS at its ownership check, which says so before the cause.
            let screen = fixture.run(&["screen", ID, "--json"]);
            assert_eq!(screen["ok"], false, "{case}: {screen}");
            let text = screen["error"].as_str().unwrap_or_default();
            if terminal == "herdr" {
                assert_eq!(
                    text,
                    format!(
                        "the recorded terminal surface was not verified as this session's; nothing was read: {refusal}"
                    ),
                    "{case}: {screen}"
                );
            } else {
                assert_eq!(text, refusal, "{case}: {screen}");
            }
            assert_eq!(screen["failure_kind"], "unsupported", "{case}: {screen}");
            unchanged("screen");

            // Listing repairs: it leaves the session as it is.
            let listed = fixture.run(&["sessions", "--json"]);
            let row = listed
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == ID))
                .unwrap_or_else(|| panic!("{case}: {listed}"));
            assert_eq!(row["state"], state, "{case}: {listed}");
            assert_eq!(row["terminal"], terminal, "{case}: {listed}");
            unchanged("sessions");
            // The read-only commands keep working.
            for args in [&["inspect", ID, "--json"][..], &["doctor", ID, "--json"]] {
                let reply = fixture.run(args);
                assert_eq!(reply["ok"], true, "{case}, {args:?}: {reply}");
            }
            let inspected = fixture.run(&["inspect", ID, "--json"]);
            assert!(inspected.to_string().contains(state), "{case}: {inspected}");
        }
    }
}

// Reopen continues a closed source's provider conversation on this machine: a closed source
// recorded on another OS is refused before anything is read or created from it.
#[test]
fn reopen_refuses_a_closed_source_of_another_os() {
    let (os, _) = here();
    for origin in ["macOS", "Linux", "Windows"] {
        if origin == os {
            continue;
        }
        let fixture = Fixture::new("herdr", origin, "closed");
        fs::remove_file(fixture.directory().join("terminal.json")).unwrap();
        let before = fs::read_dir(fixture.root.path()).unwrap().count();
        let reply = fixture.run(&["reopen", ID, "--prompt", "continue", "--json"]);
        assert_eq!(reply["ok"], false, "{origin}: {reply}");
        assert_eq!(
            reply["error"],
            format!("a session recorded on {origin} is not available on {os}"),
            "{origin}: {reply}"
        );
        assert_eq!(reply["failure_kind"], "unsupported", "{origin}: {reply}");
        assert_eq!(fs::read_dir(fixture.root.path()).unwrap().count(), before);
    }
}
