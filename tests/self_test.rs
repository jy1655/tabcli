#[cfg(any(windows, target_os = "linux"))]
use serde_json::Value;
use std::process::Command;

#[test]
fn self_test_help_explicitly_describes_real_calls_and_terminals() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
    let human = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
    let human = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
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
        let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .env("AGENT_BRIDGE_NATIVE_STATE_DIR", &root)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.exists());
    }
}
