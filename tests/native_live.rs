#![cfg(target_os = "macos")]

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
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args([
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
            "--timeout-secs",
            "300",
            "--prompt",
            &prompt,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "native {provider} smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["provider"], provider);
    assert!(
        response["result"]
            .as_str()
            .is_some_and(|result| result.contains(&marker)),
        "unexpected native {provider} result: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[ignore = "manual live smoke: opens a real iTerm tab and requires authenticated Codex"]
fn live_native_codex_forwards_flags_and_returns_result() {
    run_native_adapter_smoke("codex");
}

#[test]
#[ignore = "manual live smoke: opens a real iTerm tab and requires authenticated Claude"]
fn live_native_claude_forwards_flags_and_returns_result() {
    run_native_adapter_smoke("claude");
}

#[test]
#[ignore = "manual live smoke: opens a real iTerm tab and requires authenticated Agy"]
fn live_native_agy_forwards_flags_and_returns_result() {
    run_native_adapter_smoke("agy");
}

#[test]
#[ignore = "manual live smoke: opens a real iTerm tab and requires authenticated Pi"]
fn live_native_pi_forwards_flags_and_returns_result() {
    run_native_adapter_smoke("pi");
}
