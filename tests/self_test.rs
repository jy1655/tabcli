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
}
