#[cfg(any(windows, target_os = "linux"))]
use serde_json::Value;
use std::process::Command;

#[test]
fn self_test_help_explicitly_describes_real_calls_and_terminals() {
    let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args(["self-test", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("self-test <codex|claude|agy|pi>"));
    assert!(help.contains("makes real model calls and opens a real terminal"));
    assert!(help.contains("ordinary state root"));
    assert!(help.contains("closes only the session it creates"));
    assert!(help.contains("[--isolated]"));
    assert!(help.contains("settings and consent records do not apply"));
    assert!(help.contains("private directory stays until you remove it"));
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn unsupported_terminal_reports_failure_without_creating_a_session() {
    let root = tempfile::tempdir().unwrap();
    let foreign = root.path().join("session-foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("preserve"), b"unchanged").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args(["self-test", "claude", "--terminal", "terminal", "--json"])
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "unsupported");
    assert_eq!(report["isolated"], false);
    assert!(report["session"].is_null());
    assert_eq!(report["steps"][4]["outcome"], "not_verified");
    let state = std::path::Path::new(report["state_root"].as_str().unwrap());
    assert_eq!(state, root.path());
    assert!(report.get("state_directory").is_none());
    assert_eq!(std::fs::read_dir(state).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(foreign.join("preserve")).unwrap(),
        b"unchanged"
    );
    let new_root = root.path().join("new-root");
    assert!(!new_root.exists());
    let human = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args(["self-test", "claude", "--terminal", "terminal"])
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &new_root)
        .output()
        .unwrap();
    assert!(!human.status.success());
    assert!(!new_root.exists());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("ask: unsupported"));
    assert!(human.contains("cleanup: not_verified"));
    assert!(human.contains("state root:"));
    assert!(human.contains("mode: ordinary state root"));
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn isolated_unsupported_terminal_keeps_a_private_root_below_the_configured_parent() {
    let parent = tempfile::tempdir().unwrap();
    let foreign = parent.path().join("session-foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("preserve"), b"unchanged").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args([
            "self-test",
            "claude",
            "--isolated",
            "--terminal",
            "terminal",
            "--json",
        ])
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", parent.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["isolated"], true);
    assert_eq!(report["outcome"], "unsupported");
    assert!(report["session"].is_null());
    assert_eq!(report["steps"][4]["outcome"], "passed");
    let root = std::path::Path::new(report["state_root"].as_str().unwrap());
    assert_eq!(root.parent(), Some(parent.path()));
    assert!(
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("agent-bridge-self-test-")
    );
    assert!(root.is_dir());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 2);
    assert_eq!(
        std::fs::read(foreign.join("preserve")).unwrap(),
        b"unchanged"
    );
    let human = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args([
            "self-test",
            "--isolated",
            "claude",
            "--terminal",
            "terminal",
        ])
        .env(
            "AGENT_BRIDGE_NATIVE_STATE_DIR",
            parent.path().join("new-parent"),
        )
        .output()
        .unwrap();
    assert!(!human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("mode: isolated"));
    assert!(human.contains("cleanup: passed"));
    assert_eq!(
        std::fs::read_dir(parent.path().join("new-parent"))
            .unwrap()
            .count(),
        1
    );
    // The test owns these directories; the command itself kept them after failure.
    std::fs::remove_dir(root).unwrap();
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn isolated_without_a_configured_parent_uses_the_system_temporary_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args([
            "self-test",
            "claude",
            "--terminal",
            "terminal",
            "--json",
            "--isolated",
        ])
        .env_remove("AGENT_BRIDGE_NATIVE_STATE_DIR")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["isolated"], true);
    let root = std::path::Path::new(report["state_root"].as_str().unwrap());
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    assert!(root.is_dir());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn isolated_parser_rejects_duplicates_and_unknown_options_before_creating_a_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("must-not-exist");
    for args in [
        vec!["self-test", "claude", "--isolated", "--isolated"],
        vec!["self-test", "--isolated", "claude", "--isolated"],
        vec!["self-test", "claude", "--isolated", "--unknown"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &root)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.exists());
    }
}

// Admission (#216) runs before the self-test creates an isolated root or runs `ask`, so a
// refusal needs no terminal on any platform.
struct AdmissionFixture {
    _directory: tempfile::TempDir,
    base: std::path::PathBuf,
    home: std::path::PathBuf,
    memory: std::path::PathBuf,
}

impl AdmissionFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let home = base.join("home");
        let memory = home.join(".codex").join("memories");
        std::fs::create_dir_all(&memory).unwrap();
        let memory = memory.canonicalize().unwrap();
        Self {
            _directory: directory,
            base,
            home,
            memory,
        }
    }

    fn run(&self, root: &std::path::Path, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["self-test", "codex", "--workspace"];
        args.push(self.memory.to_str().unwrap());
        args.extend(extra);
        Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("CODEX_HOME")
            .env_remove("AGENT_BRIDGE_NATIVE_SESSION_ID")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn allow(&self, root: &std::path::Path) {
        let output = Command::new(env!("CARGO_BIN_EXE_tabcli"))
            .args(["settings", "codex-memory-workspace", "allow"])
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root)
            .env("HOME", &self.home)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn ordinary_self_test_applies_the_root_setting_before_running_ask() {
    let fixture = AdmissionFixture::new();
    let root = fixture.base.join("state");
    let output = fixture.run(&root, &["--json"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["isolated"], false);
    assert_eq!(
        std::path::Path::new(report["state_root"].as_str().unwrap()),
        root
    );
    assert_eq!(report["steps"].as_array().unwrap().len(), 1);
    assert_eq!(report["steps"][0]["name"], "admission");
    assert_eq!(report["steps"][0]["outcome"], "failed");
    let reason = report["steps"][0]["reason"].as_str().unwrap();
    assert!(
        reason.ends_with(
            "codex-memory-workspace is refuse. To allow launches here, run 'tabcli settings codex-memory-workspace allow' in the same state root."
        ),
        "{reason}"
    );
    assert_eq!(report["cleanup_sessions"], serde_json::json!([]));
    assert!(!root.exists(), "a refused self-test created the state root");
}

#[test]
fn isolated_self_test_refuses_with_default_settings_even_when_the_ordinary_root_allows() {
    let fixture = AdmissionFixture::new();
    let parent = fixture.base.join("state");
    fixture.allow(&parent);
    let before: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let output = fixture.run(&parent, &["--isolated", "--json"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["isolated"], true);
    assert!(report["state_root"].is_null());
    assert_eq!(report["steps"][0]["name"], "admission");
    assert!(
        report["steps"][0]["reason"]
            .as_str()
            .unwrap()
            .ends_with("Isolated self-test uses default settings; use ordinary self-test with the allow setting to permit this workspace.")
    );
    let human = fixture.run(&parent, &["--isolated"]);
    assert!(!human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("state root: not created"), "{human}");
    assert!(human.contains("admission: failed"), "{human}");
    // No private root was created beside the ordinary records.
    let after: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(after, before);
}

// With the ordinary root's allow setting admission passes and `ask` runs; Apple Terminal
// does not exist on these platforms, so `ask` reports unsupported without a terminal.
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn ordinary_self_test_with_the_allow_setting_runs_ask() {
    let fixture = AdmissionFixture::new();
    let root = fixture.base.join("state");
    fixture.allow(&root);
    let output = fixture.run(&root, &["--terminal", "terminal", "--json"]);
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["steps"][0]["name"], "ask");
    assert_eq!(report["outcome"], "unsupported");
    assert_eq!(report["steps"].as_array().unwrap().len(), 5);
}
