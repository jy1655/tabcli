use serde_json::Value;
use std::process::{Command, Output};

fn invoke(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tabcli"))
        .args(args)
        .env("PATH", "")
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root.join("missing-state"))
        .env("HOME", root.join("missing-home"))
        .env("USERPROFILE", root.join("missing-home"))
        .env("CODEX_HOME", root.join("missing-codex"))
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn contract_is_repeatable_without_providers_or_state_and_lists_public_commands() {
    let root = tempfile::tempdir().unwrap();
    let first = invoke(root.path(), &["describe", "--json"]);
    let second = invoke(root.path(), &["describe", "--json"]);
    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stderr.is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let value: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["scope"], "build");
    assert_eq!(value["contract_version"], 1);
    assert_eq!(value["binary_version"], env!("CARGO_PKG_VERSION"));
    let commands = value["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 18);
    assert!(
        commands
            .iter()
            .all(|command| !command["name"].as_str().unwrap().starts_with("native-"))
    );
    assert_eq!(
        value["output"]["failure_kinds"],
        serde_json::json!(["delivery_uncertain", "timed_out", "unsupported"])
    );
    let selected = invoke(root.path(), &["describe", "wait", "--json"]);
    assert!(selected.status.success());
    let one: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(
        one["commands"].as_array().unwrap(),
        &vec![
            commands
                .iter()
                .find(|c| c["name"] == "wait")
                .unwrap()
                .clone()
        ]
    );
    assert_eq!(
        one["commands"][0]["features"],
        serde_json::json!(["questions", "after_question"])
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn hidden_unknown_and_invalid_requests_fail_without_mutation_then_valid_call_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let mut unknown_errors = Vec::new();
    for name in ["native-hook", "does-not-exist"] {
        let result = invoke(root.path(), &["describe", name, "--json"]);
        assert!(!result.status.success());
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["ok"], false);
        unknown_errors.push(value["error"].clone());
    }
    assert_eq!(unknown_errors[0], unknown_errors[1]);
    for args in [
        vec!["describe", "wait", "result", "--json"],
        vec!["describe", "--json", "--json"],
        vec!["describe", "--unknown", "--json"],
    ] {
        let result = invoke(root.path(), &args);
        assert!(!result.status.success(), "{args:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap()["ok"],
            false
        );
    }
    assert!(
        invoke(root.path(), &["describe", "--json", "describe"])
            .status
            .success()
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
