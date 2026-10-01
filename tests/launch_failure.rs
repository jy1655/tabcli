//! Launch failures before an owner exists must not strand the initial request (#50).
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    root: tempfile::TempDir,
    directory: PathBuf,
}

impl Fixture {
    fn new(phase: &str, expired: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-launch");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        fs::create_dir(directory.join("requests")).unwrap();
        let fixture = Self { root, directory };
        fixture.write("manifest.json", json!({
            "schema":1, "id":"session-launch", "provider":"codex", "provider_path":"codex",
            "provider_version":"0.147.0", "workspace":fixture.root.path().canonicalize().unwrap(),
            "title":"launch fixture", "model":null, "effort":null, "yolo":false, "created_unix_ms":1
        }));
        fixture.write(
            "status.json",
            json!({"state":"launching", "generation":1,
            "updated_unix_ms":1, "exit_code":null, "error":null}),
        );
        fixture.write(
            "launch.json",
            json!({"schema":1, "claim_token":"123-456-0",
            "deadline_unix_ms":if expired { 1_u128 } else { u64::MAX as u128 }, "phase":phase}),
        );
        fixture.write(
            "requests/123-456-0.json",
            json!({"schema":1, "request_id":"request-launch",
            "claim_token":"123-456-0", "event_file":"event-1.json", "created_unix_ms":1}),
        );
        fs::write(fixture.directory.join("turn.claim"), "123-456-0\n").unwrap();
        fs::write(
            fixture.directory.join("initial-prompt.txt"),
            "never submitted",
        )
        .unwrap();
        fixture
    }

    fn write(&self, name: &str, value: Value) {
        fs::write(
            self.directory.join(name),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", self.root.path())
            .output()
            .unwrap()
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&fs::read(self.directory.join("status.json")).unwrap()).unwrap()
    }

    fn sessions(&self) {
        let output = self.run(&["sessions", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn expired_unstarted_launch_fails_and_releases_only_its_claim() {
    let fixture = Fixture::new("pending", true);
    fixture.sessions();
    let status = fixture.status();
    assert_eq!(status["state"], "failed");
    assert!(status["error"].as_str().unwrap().contains("launch"));
    assert!(!fixture.directory.join("turn.claim").exists());
    assert!(fixture.directory.join("initial-prompt.txt").exists());
    assert!(!fixture.directory.join("provider-process.json").exists());
}

#[test]
fn crash_during_spawn_keeps_claim_and_reports_uncertainty() {
    let fixture = Fixture::new("spawning", true);
    fixture.sessions();
    let status = fixture.status();
    assert_eq!(status["state"], "failed");
    assert!(status["error"].as_str().unwrap().contains("uncertain"));
    assert!(fixture.directory.join("turn.claim").exists());
}

#[test]
fn result_observes_expired_launch_without_repairing_files() {
    let fixture = Fixture::new("pending", true);
    let before = fixture.status();
    let output = fixture.run(&[
        "result",
        "session-launch",
        "--request",
        "request-launch",
        "--wait",
        "--timeout-secs",
        "1",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success());
    assert_eq!(value["request_state"], "unresolved");
    assert_ne!(value["timed_out"], true);
    assert!(value["error"].as_str().unwrap().contains("launch"));
    assert_eq!(fixture.status(), before);
    assert!(fixture.directory.join("turn.claim").exists());
}

#[test]
fn early_wrapper_error_is_recorded_and_releases_the_claim() {
    let fixture = Fixture::new("pending", false);
    let output = fixture.run(&["native-session", "session-launch"]);
    assert!(!output.status.success());
    let status = fixture.status();
    assert_eq!(status["state"], "failed");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("interactive terminal")
    );
    assert_eq!(status["exit_code"], 1);
    assert!(!fixture.directory.join("turn.claim").exists());
    let log = fs::read_to_string(fixture.directory.join("launch.log")).unwrap();
    assert!(log.contains("interactive terminal"));
    assert!(log.contains("exit_code=1"));
}

#[test]
fn fresh_pending_launch_is_not_cancelled() {
    let fixture = Fixture::new("pending", false);
    let before = fixture.status();
    fixture.sessions();
    assert_eq!(fixture.status(), before);
    assert!(fixture.directory.join("turn.claim").exists());
}

#[test]
fn interrupted_pre_spawn_failure_finishes_releasing_the_claim() {
    let fixture = Fixture::new("pending", false);
    fixture.write(
        "status.json",
        json!({"state":"failed", "generation":2,
        "updated_unix_ms":2, "exit_code":null, "error":"launch timed out"}),
    );
    let before = fixture.status();
    fixture.sessions();
    assert_eq!(fixture.status(), before);
    assert!(!fixture.directory.join("turn.claim").exists());
}

#[test]
fn launch_timeout_cannot_release_a_replacement_claim() {
    let fixture = Fixture::new("pending", true);
    fs::write(fixture.directory.join("turn.claim"), "999-888-0\n").unwrap();
    let before = fixture.status();
    fixture.sessions();
    let output = fixture.run(&["native-session", "session-launch"]);
    assert!(!output.status.success());
    assert_eq!(fixture.status(), before);
    assert_eq!(
        fs::read_to_string(fixture.directory.join("turn.claim")).unwrap(),
        "999-888-0\n"
    );
}

// A Windows Terminal tab host that decided to start the console root and then stalled
// leaves a suspended root bound to a session whose launch has failed. The root has
// reached no console, so a close cannot attach to it.
#[cfg(windows)]
mod unstarted_console_root {
    use super::*;
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{CREATE_SUSPENDED, GetProcessTimes, QueryFullProcessImageNameW},
    };

    // A suspended process bound to the fixture as its console surface.
    fn bind_suspended_root(fixture: &Fixture) -> std::process::Child {
        let root = Command::new(std::env::var_os("ComSpec").unwrap())
            .args(["/d", "/c", "exit 0"])
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .unwrap();
        let handle = root.as_raw_handle();
        let mut creation = FILETIME::default();
        let mut other = [FILETIME::default(); 3];
        assert_ne!(
            unsafe {
                GetProcessTimes(
                    handle,
                    &mut creation,
                    &mut other[0],
                    &mut other[1],
                    &mut other[2],
                )
            },
            0
        );
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        assert_ne!(
            unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) },
            0
        );
        fixture.write(
            "status.json",
            json!({"state":"failed", "generation":2, "updated_unix_ms":2, "exit_code":null,
                "error":"provider launch timed out before startup was confirmed"}),
        );
        fixture.write(
            "terminal.json",
            json!({
                "terminal": "windows-console",
                "session_id": root.id().to_string(),
                "managed_session_id": "session-launch",
                "windows_process_identity": {
                    "creation_time": (u64::from(creation.dwHighDateTime) << 32)
                        | u64::from(creation.dwLowDateTime),
                    "executable_path": String::from_utf16(&path[..length as usize]).unwrap(),
                },
            }),
        );
        root
    }

    #[test]
    fn close_ends_a_bound_console_root_that_was_never_started() {
        let fixture = Fixture::new("pending", true);
        let mut root = bind_suspended_root(&fixture);

        let output = fixture.run(&["native-console-control", "close", "session-launch"]);

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(root.wait().unwrap().code(), Some(1));
    }

    // The host that never started the root is stalled, and its tab would stay open. It
    // is ended with the root, and without a failure code, so that the tab closes.
    #[test]
    fn close_ends_the_stalled_tab_host_of_a_root_that_was_never_started() {
        let fixture = Fixture::new("pending", true);
        let mut root = bind_suspended_root(&fixture);
        // Stands in for the stalled host: a process that does not end on its own.
        let host_fixture = Fixture::new("pending", true);
        let mut host = bind_suspended_root(&host_fixture);
        let host_surface: Value = serde_json::from_slice(
            &fs::read(host_fixture.directory.join("terminal.json")).unwrap(),
        )
        .unwrap();
        fixture.write(
            "console-host-process.json",
            json!({
                "schema": 1,
                "pid": host.id(),
                "identity": host_surface["windows_process_identity"],
            }),
        );

        let output = fixture.run(&["native-console-control", "close", "session-launch"]);

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(root.wait().unwrap().code(), Some(1));
        assert_eq!(host.wait().unwrap().code(), Some(0));
    }

    // A close that cannot read the host's record ends nothing: it could not tell the
    // host from the session's processes.
    #[test]
    fn close_ends_nothing_when_the_record_of_the_tab_host_cannot_be_read() {
        let fixture = Fixture::new("pending", true);
        let mut root = bind_suspended_root(&fixture);
        fs::write(fixture.directory.join("console-host-process.json"), b"{").unwrap();

        let output = fixture.run(&["native-console-control", "close", "session-launch"]);

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("the record of the Windows Terminal tab host cannot be read")
        );
        assert!(root.try_wait().unwrap().is_none(), "the root is left alone");
        root.kill().unwrap();
        root.wait().unwrap();
    }

    // Once the wrapper has recorded itself the root has run its command, and what it
    // started would be left behind by ending the root alone.
    #[test]
    fn close_does_not_end_a_root_alone_once_its_wrapper_has_run() {
        for (owner_recorded, phase) in [(true, "pending"), (false, "spawning")] {
            let fixture = Fixture::new(phase, true);
            let mut root = bind_suspended_root(&fixture);
            if owner_recorded {
                fixture.write("native-session.json", json!({"pid": 1}));
            }

            let output = fixture.run(&["native-console-control", "close", "session-launch"]);

            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("failed to attach to the managed console process")
            );
            assert!(root.try_wait().unwrap().is_none(), "the root is left alone");
            root.kill().unwrap();
            root.wait().unwrap();
        }
    }
}

#[test]
fn doctor_reports_launch_failure_instead_of_waiting_for_the_request() {
    let fixture = Fixture::new("pending", true);
    fixture.sessions();
    let before = fixture.status();
    let output = fixture.run(&["doctor", "session-launch", "--json"]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let turn = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "turn")
        .unwrap();
    assert_eq!(turn["reason_code"], "launch_failed");
    assert_eq!(fixture.status(), before);
}
