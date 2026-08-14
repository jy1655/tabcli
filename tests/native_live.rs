#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::{
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn required_provider_value(provider: &str, suffix: &str) -> String {
    let key = format!(
        "AGENT_BRIDGE_LIVE_{}_{}",
        provider.to_ascii_uppercase(),
        suffix
    );
    std::env::var(&key)
        .unwrap_or_else(|_| panic!("set {key} to a value supported by this provider/model"))
}

fn run_native_adapter_smoke(provider: &str) {
    let model = required_provider_value(provider, "MODEL");
    let effort = required_provider_value(provider, "EFFORT");
    let marker = format!(
        "AGENT_BRIDGE_NATIVE_{}_RESULT_OK",
        provider.to_ascii_uppercase()
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let title = format!("Agent Bridge live {provider} {nonce}");
    let prompt = format!("Reply with exactly this marker and nothing else: {marker}");
    let selected_terminal = std::env::var("AGENT_BRIDGE_LIVE_TERMINAL").ok();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
    command.args([
        "ask",
        provider,
        "--workspace",
        env!("CARGO_MANIFEST_DIR"),
        "--title",
        &title,
        "--model",
        &model,
        "--effort",
        &effort,
    ]);
    if let Some(terminal) = selected_terminal.as_deref() {
        command.args(["--terminal", terminal]);
    }
    let output = command
        .args(["--timeout-secs", "300", "--prompt", &prompt, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "native {provider} smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let session = response["session"]
        .as_str()
        .expect("native ask response is missing its session id");

    let close_output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["close-session", session, "--explicit", "--json"])
        .output()
        .unwrap();
    assert!(
        close_output.status.success(),
        "native {provider} close failed: {}",
        String::from_utf8_lossy(&close_output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&close_output.stdout));
    let close_response: serde_json::Value = serde_json::from_slice(&close_output.stdout).unwrap();
    assert_eq!(close_response["ok"], true);
    assert_eq!(close_response["closed"], true);
    assert_eq!(close_response["session"], session);

    let sessions_output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(
        sessions_output.status.success(),
        "native sessions lookup failed after closing {session}: {}",
        String::from_utf8_lossy(&sessions_output.stderr)
    );
    let sessions: serde_json::Value = serde_json::from_slice(&sessions_output.stdout).unwrap();
    let closed_session = sessions
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == session))
        .expect("closed native session is missing from the session registry");
    assert_eq!(closed_session["state"], "closed");

    assert_eq!(response["provider"], provider);
    assert!(
        matches!(
            response["terminal"].as_str(),
            Some("ghostty" | "iterm2" | "apple-terminal" | "windows-console")
        ),
        "native response is missing a supported terminal kind: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        response["terminal_session_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "native response is missing its terminal session id: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        response["result"]
            .as_str()
            .is_some_and(|result| result.contains(&marker)),
        "unexpected native {provider} result: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[ignore = "manual live smoke: opens and closes a supported terminal surface and requires authenticated Codex"]
fn live_native_codex_returns_result_and_closes_session() {
    run_native_adapter_smoke("codex");
}

#[test]
#[ignore = "manual live smoke: opens and closes a supported terminal surface and requires authenticated Claude"]
fn live_native_claude_returns_result_and_closes_session() {
    run_native_adapter_smoke("claude");
}

#[test]
#[ignore = "manual live smoke: opens and closes a supported terminal surface and requires authenticated Agy"]
fn live_native_agy_returns_result_and_closes_session() {
    run_native_adapter_smoke("agy");
}

#[test]
#[ignore = "manual live smoke: opens and closes a supported terminal surface and requires authenticated Pi"]
fn live_native_pi_returns_result_and_closes_session() {
    run_native_adapter_smoke("pi");
}
