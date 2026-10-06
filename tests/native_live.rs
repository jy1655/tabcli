#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
fn shell_quote(value: &OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

#[cfg(windows)]
fn cmd_environment_argument(value: &OsStr) -> String {
    let mut value = value.to_string_lossy().into_owned();
    let trailing_backslashes = value
        .chars()
        .rev()
        .take_while(|value| *value == '\\')
        .count();
    value.extend(std::iter::repeat_n('\\', trailing_backslashes));
    value
}

fn run_bridge(arguments: &[&OsStr]) -> Output {
    let executable = Path::new(env!("CARGO_BIN_EXE_tabcli"));
    #[cfg(windows)]
    if let Ok(shell) = std::env::var("AGENT_BRIDGE_LIVE_CALLER_SHELL") {
        let mut command = match shell.as_str() {
            "cmd" => {
                use std::os::windows::process::CommandExt;

                let mut command = Command::new("cmd.exe");
                command.env("AGENT_BRIDGE_LIVE_EXE", executable);
                let mut references = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    assert!(!argument.to_string_lossy().contains('"'));
                    let name = format!("AGENT_BRIDGE_LIVE_ARG_{index}");
                    command.env(&name, cmd_environment_argument(argument));
                    references.push(format!("\"%{name}%\""));
                }
                let line = format!("\"%AGENT_BRIDGE_LIVE_EXE%\" {}", references.join(" "));
                command.args(["/d", "/v:off", "/s", "/c"]);
                command.raw_arg(format!(" \"{line}\""));
                command
            }
            "windows-powershell" | "pwsh" => {
                let invocation =
                    std::iter::once(format!("& {}", shell_quote(executable.as_os_str())))
                        .chain(arguments.iter().map(|value| shell_quote(value)))
                        .collect::<Vec<_>>()
                        .join(" ");
                let line = format!(
                    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; {invocation}"
                );
                let program = if shell == "windows-powershell" {
                    "powershell.exe"
                } else {
                    "pwsh.exe"
                };
                let mut command = Command::new(program);
                command.args(["-NoLogo", "-NoProfile", "-Command", &line]);
                command
            }
            other => panic!("unsupported AGENT_BRIDGE_LIVE_CALLER_SHELL: {other}"),
        };
        return command.output().unwrap();
    }
    Command::new(executable).args(arguments).output().unwrap()
}

fn required_provider_value(provider: &str, suffix: &str) -> String {
    let key = format!(
        "AGENT_BRIDGE_LIVE_{}_{}",
        provider.to_ascii_uppercase(),
        suffix
    );
    std::env::var(&key)
        .unwrap_or_else(|_| panic!("set {key} to a value supported by this provider/model"))
}

fn wait_for_detached_result(session: &str, minimum_results: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let output = run_bridge(&[OsStr::new("sessions"), OsStr::new("--json")]);
        assert!(
            output.status.success(),
            "native sessions lookup failed while waiting for detached {session}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let sessions: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let current = sessions
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == session))
            .expect("detached native session is missing from the session registry");
        if current["state"] == "ready"
            && current["results"]
                .as_u64()
                .is_some_and(|results| results >= minimum_results)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "detached native session {session} did not become ready with {minimum_results} results: {current}"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(windows)]
#[test]
fn cmd_environment_arguments_double_only_trailing_backslashes() {
    assert_eq!(
        cmd_environment_argument(OsStr::new("plain\\path")),
        "plain\\path"
    );
    assert_eq!(cmd_environment_argument(OsStr::new("C:\\")), "C:\\\\");
    assert_eq!(
        cmd_environment_argument(OsStr::new("two\\\\")),
        "two\\\\\\\\"
    );
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
    let title = format!("Agent Bridge live {provider} {nonce} 한글");
    let prompt = format!("Reply with exactly this marker and nothing else: {marker}");
    let prompt_directory = tempfile::Builder::new()
        .prefix("Agent Bridge LIVE prompts ")
        .tempdir()
        .unwrap();
    let initial_prompt_path = prompt_directory.path().join("초기 prompt 입력.txt");
    std::fs::write(&initial_prompt_path, &prompt).unwrap();
    let selected_terminal = std::env::var("AGENT_BRIDGE_LIVE_TERMINAL").ok();
    let mut arguments = vec![
        OsStr::new("ask"),
        OsStr::new(provider),
        OsStr::new("--workspace"),
        OsStr::new(env!("CARGO_MANIFEST_DIR")),
        OsStr::new("--title"),
        OsStr::new(&title),
        OsStr::new("--model"),
        OsStr::new(&model),
        OsStr::new("--effort"),
        OsStr::new(&effort),
    ];
    if let Some(terminal) = selected_terminal.as_deref() {
        arguments.extend([OsStr::new("--terminal"), OsStr::new(terminal)]);
    }
    arguments.extend([
        OsStr::new("--timeout-secs"),
        OsStr::new("300"),
        OsStr::new("--prompt-file"),
        initial_prompt_path.as_os_str(),
        OsStr::new("--json"),
    ]);
    let output = run_bridge(&arguments);
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

    let detached_marker = format!("{marker}_DETACHED");
    let detached_prompt =
        format!("Reply with exactly this marker and nothing else: {detached_marker}");
    let detached_prompt_path = prompt_directory.path().join("분리 후속 prompt 입력.txt");
    std::fs::write(&detached_prompt_path, &detached_prompt).unwrap();
    let detached_output = run_bridge(&[
        OsStr::new("tell"),
        OsStr::new(session),
        OsStr::new("--timeout-secs"),
        OsStr::new("300"),
        OsStr::new("--prompt-file"),
        detached_prompt_path.as_os_str(),
        OsStr::new("--detach"),
        OsStr::new("--json"),
    ]);
    assert!(
        detached_output.status.success(),
        "native {provider} detached follow-up failed: {}",
        String::from_utf8_lossy(&detached_output.stderr)
    );
    let detached_response: serde_json::Value =
        serde_json::from_slice(&detached_output.stdout).unwrap();
    assert_eq!(detached_response["session"], session);
    assert_eq!(detached_response["result"], serde_json::Value::Null);
    wait_for_detached_result(session, 2, Duration::from_secs(300));

    let follow_up_marker = format!("{marker}_FOLLOW_UP");
    let follow_up_prompt =
        format!("Reply with exactly this marker and nothing else: {follow_up_marker}");
    let follow_up_prompt_path = prompt_directory.path().join("후속 prompt 입력.txt");
    std::fs::write(&follow_up_prompt_path, &follow_up_prompt).unwrap();
    let tell_output = run_bridge(&[
        OsStr::new("tell"),
        OsStr::new(session),
        OsStr::new("--timeout-secs"),
        OsStr::new("300"),
        OsStr::new("--prompt-file"),
        follow_up_prompt_path.as_os_str(),
        OsStr::new("--json"),
    ]);
    assert!(
        tell_output.status.success(),
        "native {provider} follow-up failed: {}",
        String::from_utf8_lossy(&tell_output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&tell_output.stdout));
    let tell_response: serde_json::Value = serde_json::from_slice(&tell_output.stdout).unwrap();
    assert_eq!(tell_response["session"], session);
    assert!(
        tell_response["result"]
            .as_str()
            .is_some_and(|result| result.contains(&follow_up_marker)),
        "unexpected native {provider} follow-up result: {}",
        String::from_utf8_lossy(&tell_output.stdout)
    );

    let close_output = run_bridge(&[
        OsStr::new("close-session"),
        OsStr::new(session),
        OsStr::new("--explicit"),
        OsStr::new("--json"),
    ]);
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

    let repeated_close_output = run_bridge(&[
        OsStr::new("close-session"),
        OsStr::new(session),
        OsStr::new("--explicit"),
        OsStr::new("--json"),
    ]);
    assert!(
        repeated_close_output.status.success(),
        "repeated native {provider} close failed: {}",
        String::from_utf8_lossy(&repeated_close_output.stderr)
    );
    let repeated_close_response: serde_json::Value =
        serde_json::from_slice(&repeated_close_output.stdout).unwrap();
    assert_eq!(repeated_close_response["ok"], true);
    assert_eq!(repeated_close_response["closed"], true);
    assert_eq!(repeated_close_response["session"], session);

    let sessions_output = run_bridge(&[OsStr::new("sessions"), OsStr::new("--json")]);
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
