use super::*;
use crate::native::provider::LaunchContext;
use crate::native::provider::codex::{self, test_support};
use std::ffi::OsString;

const WINDOWS_EXE: &str = r"C:\Users\user\.cargo\bin\tabcli.exe";
const WINDOWS_STOP: &str =
    "sha256:3d4b3bb1f8745bcb422e18c887e69171ed33a5ad43a2f509e2088dfd636cf5b2";
const WINDOWS_UPS: &str = "sha256:9176bae767677554876e4599d643ab554ded5dcb47b8b167ca5d7ccf9917de01";

const MACOS_EXE: &str = "/Users/jyh/.local/bin/tabcli";
const MACOS_STOP: &str = "sha256:9a3d2ddbafef699ee6addaf9ec24e5c00cf4beaec6af55e0cae219bf28273efe";
const MACOS_UPS: &str = "sha256:e34dae680a24bd17287545820261c0a98b9118ca447ddabc23e9cb12f0226d45";

#[test]
fn trust_hash_matches_known_sha256_of_fixed_identity() {
    // SHA-256 of this exact sorted JSON identity, including its command quotes.
    let identity = r#"{"event_name":"stop","hooks":[{"async":false,"command":"'/Users/jyh/.local/bin/tabcli' native-hook codex","timeout":30,"type":"command"}]}"#;
    let identity: serde_json::Value = serde_json::from_str(identity).unwrap();
    assert_eq!(
        trust_hash(
            identity["event_name"].as_str().unwrap(),
            identity["hooks"][0]["command"].as_str().unwrap()
        ),
        MACOS_STOP
    );
}

// Identity hashing is host-independent, so the Mac vectors run on Windows too.
#[test]
fn trust_hash_matches_both_codex_0_163_macos_vectors() {
    let command = format!("'{MACOS_EXE}' native-hook codex");
    assert_eq!(trust_hash("stop", &command), MACOS_STOP);
    assert_eq!(trust_hash("user_prompt_submit", &command), MACOS_UPS);
}

#[test]
fn trust_hash_matches_both_codex_0_163_windows_vectors() {
    let command = format!("{WINDOWS_EXE} native-hook codex");
    assert_eq!(trust_hash("stop", &command), WINDOWS_STOP);
    assert_eq!(trust_hash("user_prompt_submit", &command), WINDOWS_UPS);
    assert_ne!(
        trust_hash("stop", &format!("{command} changed")),
        WINDOWS_STOP
    );
    assert_ne!(trust_hash("changed", &command), WINDOWS_STOP);
}

fn assert_override_parses(executable: &Path) {
    let value = hooks_override(executable).unwrap();
    let parsed: toml::Value = value.parse().unwrap();
    let hooks = &parsed["hooks"];
    assert_eq!(hooks.as_table().unwrap().len(), 3);
    assert_eq!(hooks["state"].as_table().unwrap().len(), 2);
    let command = hook_command(executable).unwrap();
    for (event, label) in [("Stop", "stop"), ("UserPromptSubmit", "user_prompt_submit")] {
        assert_eq!(
            hooks[event][0]["hooks"][0]["command"].as_str(),
            Some(command.as_str())
        );
        let state = &hooks["state"][session_flags_state_key(label)];
        assert_eq!(state.as_table().unwrap().len(), 1);
        assert!(
            state.get("enabled").is_none(),
            "leave user enabled=false untouched by field-wise merge"
        );
        assert_eq!(
            state["trusted_hash"].as_str(),
            Some(trust_hash(label, &command).as_str())
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_override_is_exact_quote_free_toml_and_uses_verified_state_keys() {
    assert_eq!(
        session_flags_state_key("stop"),
        r"C:\<session-flags>\config.toml:stop:0:0"
    );
    let expected = concat!(
        "hooks={Stop=[{hooks=[{type='command',command='C:\\Users\\user\\.cargo\\bin\\tabcli.exe native-hook codex',timeout=30,async=false}]}],",
        "UserPromptSubmit=[{hooks=[{type='command',command='C:\\Users\\user\\.cargo\\bin\\tabcli.exe native-hook codex',timeout=30,async=false}]}],",
        "state={'C:\\<session-flags>\\config.toml:stop:0:0'={trusted_hash='sha256:3d4b3bb1f8745bcb422e18c887e69171ed33a5ad43a2f509e2088dfd636cf5b2'},",
        "'C:\\<session-flags>\\config.toml:user_prompt_submit:0:0'={trusted_hash='sha256:9176bae767677554876e4599d643ab554ded5dcb47b8b167ca5d7ccf9917de01'}}}"
    );
    let actual = hooks_override(Path::new(WINDOWS_EXE)).unwrap();
    assert_eq!(actual, expected);
    assert!(!actual.contains('"'));
    assert_override_parses(Path::new(WINDOWS_EXE));
}

#[cfg(windows)]
#[test]
fn windows_launcher_refuses_spaces_unicode_quotes_and_shell_metacharacters() {
    assert_eq!(
        hook_command(Path::new(WINDOWS_EXE)),
        Some(format!("{WINDOWS_EXE} native-hook codex"))
    );
    for path in [
        r"C:\space dir\tabcli.exe",
        r"C:\한글\tabcli.exe",
        r"C:\a'b\tabcli.exe",
        "C:\\a\"b.exe",
        r"C:\a&b.exe",
        r"C:\a%b.exe",
        "",
    ] {
        assert_eq!(hook_command(Path::new(path)), None, "{path}");
        assert_eq!(hooks_override(Path::new(path)), None, "{path}");
    }
}

#[cfg(unix)]
#[test]
fn unix_override_is_exact_basic_string_toml_and_uses_verified_macos_state_keys() {
    assert_eq!(
        session_flags_state_key("stop"),
        "/<session-flags>/config.toml:stop:0:0"
    );
    let exe = Path::new(MACOS_EXE);
    let command = "'/Users/jyh/.local/bin/tabcli' native-hook codex";
    assert_eq!(hook_command(exe).as_deref(), Some(command));
    let expected = format!(
        "hooks={{Stop=[{{hooks=[{{type='command',command=\"'/Users/jyh/.local/bin/tabcli' native-hook codex\",timeout=30,async=false}}]}}],UserPromptSubmit=[{{hooks=[{{type='command',command=\"'/Users/jyh/.local/bin/tabcli' native-hook codex\",timeout=30,async=false}}]}}],state={{'/<session-flags>/config.toml:stop:0:0'={{trusted_hash='{}'}},'/<session-flags>/config.toml:user_prompt_submit:0:0'={{trusted_hash='{}'}}}}}}",
        MACOS_STOP, MACOS_UPS
    );
    assert_eq!(hooks_override(exe).unwrap(), expected);
    assert_override_parses(exe);
    let unusual = Path::new("/opt/a'b\\c\"한글😀\n\t");
    assert_eq!(
        hook_command(unusual).unwrap(),
        "'/opt/a'\"'\"'b\\c\"한글😀\n\t' native-hook codex"
    );
    assert_override_parses(unusual);
}

#[test]
fn launch_arguments_and_record_cover_version_settings_and_launcher_gates() {
    let available = if cfg!(windows) {
        WINDOWS_EXE
    } else {
        "/opt/tabcli"
    };
    let unavailable = if cfg!(windows) {
        r"C:\space dir\tabcli.exe"
    } else {
        ""
    };
    for (version, setting, exe, reason) in [
        (
            Some("codex-cli 0.147.0"),
            "on",
            available,
            Some("version_below_0_163"),
        ),
        (
            Some("0.148.0"),
            "on",
            available,
            Some("version_below_0_163"),
        ),
        (
            Some("0.149.0"),
            "on",
            available,
            Some("version_below_0_163"),
        ),
        (
            Some("0.162.9"),
            "on",
            available,
            Some("version_below_0_163"),
        ),
        (Some("0.163.0"), "on", available, None),
        (
            Some("0.163.0-alpha.1"),
            "on",
            available,
            Some("version_below_0_163"),
        ),
        (Some("0.163.1-alpha.1"), "on", available, Some("prerelease")),
        (Some("0.164.0-alpha.1"), "on", available, Some("prerelease")),
        (Some("0.163.0+build.7"), "on", available, None),
        (Some("codex-cli 0.164.0"), "on", available, None),
        (
            Some("unparseable"),
            "on",
            available,
            Some("version_unknown"),
        ),
        (None, "on", available, Some("version_unknown")),
        (Some("0.163.0"), "off", available, Some("settings_off")),
        (
            Some("0.163.0"),
            "on",
            unavailable,
            Some("launcher_unavailable"),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), version);
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(&serde_json::json!({"schema":1,"codex_lifecycle_hooks":setting}))
            .unwrap();
        let executable = Path::new(exe);
        let prompt = "[Agent Bridge native delegation]\nSource: test\n\nhello";
        let plan = codex::prepare_launch_for_platform(
            LaunchContext {
                bridge_executable: executable,
                directory: &directory,
                workspace: root.path(),
                title: "test",
                prompt,
                yolo: false,
                model: None,
                effort: None,
            },
            cfg!(windows),
            |_, _| Ok(false),
        )
        .unwrap();
        let pending = codex::read_pending_turn(&directory).unwrap().unwrap();
        let mut expected = vec![
            OsString::from("-c"),
            OsString::from(format!(
                "notify={}",
                serde_json::to_string(&[exe, "native-hook", "codex"]).unwrap()
            )),
        ];
        if reason.is_none() {
            expected.extend([
                OsString::from("-c"),
                OsString::from(hooks_override(executable).unwrap()),
            ]);
        }
        if !cfg!(windows) {
            expected.extend([
                OsString::from("-C"),
                root.path().as_os_str().to_owned(),
                OsString::from(codex::correlated_prompt(prompt, &pending)),
            ]);
        }
        assert_eq!(plan.arguments, expected, "{version:?} {setting} {exe}");
        let mode = read_launch_mode(&directory).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(mode).unwrap(),
            serde_json::json!({
                "schema":1, "lifecycle_hooks":if reason.is_none() {"injected"} else {"not_injected"},
                "reason":reason, "codex_version":version, "vector_codex_version":"0.163.0"
            })
        );
        // A later settings change cannot rewrite a launch's recorded decision.
        let before = std::fs::read(directory.join(FILE)).unwrap();
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(&serde_json::json!({"schema":1,"codex_lifecycle_hooks":"off"}))
            .unwrap();
        assert!(read_launch_mode(&directory).unwrap().is_some());
        assert_eq!(std::fs::read(directory.join(FILE)).unwrap(), before);
    }
}

#[test]
fn launch_mode_reader_rejects_invalid_schema_values_and_inconsistent_reason() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(read_launch_mode(temp.path()).unwrap(), None);
    let valid = serde_json::json!({"schema":1,"lifecycle_hooks":"injected","reason":null,"codex_version":"0.163.0","vector_codex_version":"0.163.0"});
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("lifecycle_hooks", serde_json::json!("maybe")),
        ("reason", serde_json::json!("settings_off")),
        ("reason", serde_json::json!("prerelease")),
        ("vector_codex_version", serde_json::json!("other")),
        ("codex_version", serde_json::json!(3)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        Store::open_unchecked(temp.path())
            .private(FILE)
            .write_json(&invalid)
            .unwrap();
        assert!(read_launch_mode(temp.path()).is_err(), "{invalid}");
    }
    let prerelease = serde_json::json!({"schema":1,"lifecycle_hooks":"not_injected","reason":"prerelease","codex_version":"0.164.0-alpha.1","vector_codex_version":"0.163.0"});
    Store::open_unchecked(temp.path())
        .private(FILE)
        .write_json(&prerelease)
        .unwrap();
    let read = read_launch_mode(temp.path()).unwrap().unwrap();
    assert_eq!(read.reason, Some(Reason::Prerelease));
    assert_eq!(serde_json::to_value(read).unwrap(), prerelease);
    std::fs::write(temp.path().join(FILE), b"{").unwrap();
    assert!(read_launch_mode(temp.path()).is_err());
}

#[cfg(windows)]
fn run_shell_hook(
    shell: &Path,
    is_cmd: bool,
    line: &str,
    root: &Path,
    directory: &Path,
    payload: Vec<u8>,
) -> std::process::Output {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut command = Command::new(shell);
    if is_cmd {
        use std::os::windows::process::CommandExt;
        command.arg("/C").raw_arg(format!("\"{line}\""));
    } else {
        command.args(["-NoProfile", "-Command", line]);
    }
    let mut child = command
        .env("AGENT_BRIDGE_NATIVE_STATE_DIR", root)
        .env("AGENT_BRIDGE_NATIVE_SESSION_DIR", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&payload));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("{} hook did not exit within 30 seconds", shell.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    writer.join().unwrap().unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(windows)]
fn assert_unicode_result(actual: &str, expected: &str) {
    assert!(
        actual == expected,
        "recorded result changed Unicode content"
    );
    assert_eq!(
        actual.len(),
        expected.len(),
        "recorded UTF-8 byte length changed"
    );
}

#[cfg(windows)]
fn assert_real_shell_preserves_large_stdin(is_cmd: bool) {
    use crate::native::session::{SessionState, turn, update_status};
    use std::path::PathBuf;
    // Record PATH once at test start. Resolve cmd through SystemRoot\System32,
    // pwsh.exe through an absolute entry in this PATH snapshot, then the standard
    // Program Files\PowerShell\7 location. Every spawn uses the selected full path.
    let recorded_path = std::env::var_os("PATH").unwrap_or_default();
    let test_executable = std::env::current_exe().unwrap();
    let executable = test_executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tabcli.exe");
    // No ambient binary override: all-targets builds this profile's sibling binary.
    assert!(
        executable.is_file(),
        "missing {}: run cargo test --all-targets to build the required sibling tabcli.exe",
        executable.display()
    );
    let path = executable.to_string_lossy();
    if path.contains(' ') || !path.is_ascii() {
        eprintln!(
            "SKIP {} shell hook test: build path contains a space or non-ASCII and the Windows hook launcher does not support it: {}",
            if is_cmd { "cmd" } else { "pwsh" },
            executable.display()
        );
        return;
    }
    let line = hook_command(&executable)
        .expect("test binary path must satisfy the Windows hook launcher whitelist");
    let shell = if is_cmd {
        PathBuf::from(
            std::env::var_os("SystemRoot").expect("SystemRoot is required to locate cmd.exe"),
        )
        .join("System32")
        .join("cmd.exe")
    } else {
        std::env::split_paths(&recorded_path)
            .filter(|entry| entry.is_absolute())
            .map(|entry| entry.join("pwsh.exe"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                PathBuf::from(
                    std::env::var_os("ProgramFiles")
                        .expect("ProgramFiles is required for the pwsh.exe fallback"),
                )
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe")
            })
    };
    assert!(
        shell.is_absolute() && shell.is_file(),
        "required shell not found at {} (pwsh uses the PATH snapshot, then Program Files)",
        shell.display()
    );

    // Preserve the parsed-input failure case: it proves exit 1, not exit 2, and
    // empty stdout. Only the successful round trips below prove Unicode fidelity.
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("0.163.0"));
    let expected = "한글😀".repeat(5000);
    assert!(expected.len() > 32_000);
    let payload = serde_json::to_vec(&serde_json::json!({
        "type":"agent-turn-complete", "session_id":"test", "turn_id":"test",
        "last_assistant_message": expected, "stop_hook_active": false
    }))
    .unwrap();
    assert!(payload.len() > 32_000);
    let output = run_shell_hook(&shell, is_cmd, &line, root.path(), &directory, payload);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}: {stderr}",
        shell.display()
    );
    assert!(output.stdout.is_empty());
    assert!(
        stderr.contains("Codex notify payload has no assistant result"),
        "{stderr}"
    );

    // Each case has its own claim/session: both traverse the real parser, existing
    // Codex correlation, and publication, then read back through session readers.
    for altered in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), Some("0.163.0"));
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let reader = Reader::open_unchecked(&directory);
        let claim = turn::current_claim_token(&reader).unwrap().unwrap();
        let prompt =
            crate::native::native_delegation_prompt("external", "Reply in Korean with emoji.");
        let pending = codex::install_pending_turn_for_prompt(&directory, &claim, &prompt).unwrap();
        let framed = codex::correlated_prompt(&prompt, &pending);
        assert!(framed.ends_with(&pending.marker));
        let sent = if altered {
            expected.replace('😀', "?")
        } else {
            expected.clone()
        };
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "12345678-1234-1234-1234-123456789abc",
            "turn-id": "unicode-round-trip", "input-messages": [framed],
            "last-assistant-message": format!("{sent}\n{}", pending.marker)
        }))
        .unwrap();
        assert!(payload.len() > 32_000);
        let output = run_shell_hook(&shell, is_cmd, &line, root.path(), &directory, payload);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}: {}",
            shell.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        let events = reader.events().unwrap();
        assert_eq!(events.len(), 1);
        let event = reader
            .event_strict(events[0].file_name().unwrap().to_str().unwrap())
            .unwrap();
        assert_eq!(
            event.provider_session_id.as_deref(),
            Some("12345678-1234-1234-1234-123456789abc")
        );
        assert_eq!(event.turn_id.as_deref(), Some("unicode-round-trip"));
        if altered {
            // Prove the same full-content assertion rejects valid-JSON corruption,
            // not merely that the altered payload is still parseable.
            assert_unicode_result(&event.message, &sent);
            assert!(
                std::panic::catch_unwind(|| assert_unicode_result(&event.message, &expected))
                    .is_err()
            );
            assert_ne!(event.message.len(), expected.len());
        } else {
            assert_unicode_result(&event.message, &expected);
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_cmd_hook_preserves_large_unicode_results_and_nonblocking_errors() {
    assert_real_shell_preserves_large_stdin(true);
}

#[cfg(windows)]
#[test]
fn windows_pwsh_hook_preserves_large_unicode_results_and_nonblocking_errors() {
    assert_real_shell_preserves_large_stdin(false);
}

#[cfg(windows)]
#[test]
fn windows_lifecycle_hook_binary_never_blocks_or_writes_stdout() {
    use crate::native::provider::codex;
    use crate::native::session::{SessionState, turn, update_status};
    let exe = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tabcli.exe");
    assert!(exe.is_file(), "all-targets must build tabcli.exe");
    let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    let Some(line) = hook_command(&exe) else {
        eprintln!("SKIP lifecycle shell test: executable path outside the injection whitelist");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("0.163.0"));
    update_status(&directory, SessionState::Working, None, None).unwrap();
    let token = turn::current_claim_token(&Reader::open_unchecked(&directory))
        .unwrap()
        .unwrap();
    let pending = codex::install_pending_turn(&directory, &token).unwrap();
    let prompt = codex::correlated_prompt(
        &crate::native::native_delegation_prompt("external", &"한글😀".repeat(5000)),
        &pending,
    );
    let thread = "01234567-89ab-cdef-0123-456789abcdef";
    for (payload, expected) in [
        (serde_json::to_vec(&serde_json::json!({"hook_event_name":"UserPromptSubmit", "session_id":thread, "turn_id":"turn", "transcript_path":"rollout.jsonl", "prompt":prompt})).unwrap(), 0),
        (serde_json::to_vec(&serde_json::json!({"hook_event_name":"Stop", "session_id":thread, "turn_id":"turn", "last_assistant_message":"한글😀", "stop_hook_active":false})).unwrap(), 0),
        (br#"{"hook_event_name":"FutureEvent"}"#.to_vec(), 0),
        (b"{".to_vec(), 1),
    ] {
        let output = run_shell_hook(&shell, true, &line, root.path(), &directory, payload);
        assert_eq!(output.status.code(), Some(expected), "{}", String::from_utf8_lossy(&output.stderr));
        assert_ne!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    let binding: serde_json::Value = Reader::open_unchecked(&directory)
        .private(&format!("codex-turn-{token}.json"))
        .json()
        .unwrap();
    assert_eq!(binding["thread_id"], thread);
    let candidate: serde_json::Value = Reader::open_unchecked(&directory)
        .private(&format!("codex-candidate-{token}.json"))
        .json()
        .unwrap();
    assert_eq!(candidate["last_assistant_message"], "한글😀");
    assert!(
        Reader::open_unchecked(&directory)
            .events()
            .unwrap()
            .is_empty()
    );
}
