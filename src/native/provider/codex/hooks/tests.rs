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
// Reported by `hooks/list` of codex-cli 0.166.0 on the Mac (2026-11-05) for the PreToolUse group
// with matcher `request_user_input_async`, as `trusted`; the same group with the matcher-less
// hash below was reported `modified`. The matcher-less group itself is `trusted` with that hash.
const MACOS_QUESTION: &str =
    "sha256:e8f7d8cc0bdc38499f9bcad8d3039fb23ddbf70ef78121f3a86a045f83e15254";
const MACOS_QUESTION_WITHOUT_MATCHER: &str =
    "sha256:ccecf0571c44dd24fc5baa1b55d4057e13f9923c2d8ba9920bc720083cba7008";
// Computed by the same normalization, which reproduces the Windows vectors above; Codex has not
// reported this value on Windows.
const WINDOWS_QUESTION: &str =
    "sha256:6c3f7d57ef1b37b59431237a9fe281c3488c83ce4f202d5f9a81fc45d66c800c";

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

#[test]
fn notice_hook_hash_includes_its_matcher_and_matches_the_codex_0_166_vectors() {
    let macos = format!("'{MACOS_EXE}' native-hook codex");
    assert_eq!(
        matched_trust_hash("pre_tool_use", Some(QUESTION_TOOL), &macos),
        MACOS_QUESTION
    );
    assert_eq!(
        trust_hash("pre_tool_use", &macos),
        MACOS_QUESTION_WITHOUT_MATCHER
    );
    assert_eq!(
        matched_trust_hash("pre_tool_use", None, &macos),
        MACOS_QUESTION_WITHOUT_MATCHER
    );
    let windows = format!("{WINDOWS_EXE} native-hook codex");
    assert_eq!(
        matched_trust_hash("pre_tool_use", Some(QUESTION_TOOL), &windows),
        WINDOWS_QUESTION
    );
    // The lifecycle vectors are unchanged by the matcher-aware identity.
    assert_eq!(matched_trust_hash("stop", None, &macos), MACOS_STOP);
    assert_eq!(matched_trust_hash("stop", None, &windows), WINDOWS_STOP);
    assert_ne!(
        matched_trust_hash("pre_tool_use", Some("request_user_input"), &macos),
        MACOS_QUESTION
    );
}

// Codex 0.166.0 treats a pattern of ASCII letters, digits, `_` and `|` as exact alternatives and
// anything else as a regex (hooks/src/engine/matcher.rs). The question tool's pattern must stay
// in the exact class so that it selects that one tool name and never `request_user_input`.
#[test]
fn question_matcher_is_an_exact_pattern_for_the_question_tool_only() {
    assert_eq!(QUESTION_TOOL, "request_user_input_async");
    assert!(
        QUESTION_TOOL
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
    let exact = |input: &str| QUESTION_TOOL.split('|').any(|candidate| candidate == input);
    assert!(exact("request_user_input_async"));
    for other in [
        "request_user_input",
        "request_user_input_async_v2",
        "Bash",
        "",
    ] {
        assert!(!exact(other), "{other}");
    }
}

fn assert_override_parses(executable: &Path) {
    for notice in [false, true] {
        assert_override_parses_with(executable, notice);
    }
}

fn assert_override_parses_with(executable: &Path, notice: bool) {
    let value = hooks_override(executable, notice).unwrap();
    let parsed: toml::Value = value.parse().unwrap();
    let hooks = &parsed["hooks"];
    assert_eq!(hooks.as_table().unwrap().len(), if notice { 4 } else { 3 });
    assert_eq!(
        hooks["state"].as_table().unwrap().len(),
        if notice { 3 } else { 2 }
    );
    let command = hook_command(executable).unwrap();
    if notice {
        let group = &hooks["PreToolUse"];
        assert_eq!(group.as_array().unwrap().len(), 1);
        assert_eq!(group[0]["matcher"].as_str(), Some(QUESTION_TOOL));
        assert_eq!(
            group[0]["hooks"][0]["command"].as_str(),
            Some(command.as_str())
        );
        let state = &hooks["state"][session_flags_state_key("pre_tool_use")];
        assert_eq!(state.as_table().unwrap().len(), 1);
        assert_eq!(
            state["trusted_hash"].as_str(),
            Some(matched_trust_hash("pre_tool_use", Some(QUESTION_TOOL), &command).as_str())
        );
    } else {
        assert!(hooks.get("PreToolUse").is_none());
    }
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
    let actual = hooks_override(Path::new(WINDOWS_EXE), false).unwrap();
    assert_eq!(actual, expected);
    assert!(!actual.contains('"'));
    let notice = concat!(
        "hooks={Stop=[{hooks=[{type='command',command='C:\\Users\\user\\.cargo\\bin\\tabcli.exe native-hook codex',timeout=30,async=false}]}],",
        "UserPromptSubmit=[{hooks=[{type='command',command='C:\\Users\\user\\.cargo\\bin\\tabcli.exe native-hook codex',timeout=30,async=false}]}],",
        "PreToolUse=[{matcher='request_user_input_async',hooks=[{type='command',command='C:\\Users\\user\\.cargo\\bin\\tabcli.exe native-hook codex',timeout=30,async=false}]}],",
        "state={'C:\\<session-flags>\\config.toml:stop:0:0'={trusted_hash='sha256:3d4b3bb1f8745bcb422e18c887e69171ed33a5ad43a2f509e2088dfd636cf5b2'},",
        "'C:\\<session-flags>\\config.toml:user_prompt_submit:0:0'={trusted_hash='sha256:9176bae767677554876e4599d643ab554ded5dcb47b8b167ca5d7ccf9917de01'},",
        "'C:\\<session-flags>\\config.toml:pre_tool_use:0:0'={trusted_hash='sha256:6c3f7d57ef1b37b59431237a9fe281c3488c83ce4f202d5f9a81fc45d66c800c'}}}"
    );
    let actual = hooks_override(Path::new(WINDOWS_EXE), true).unwrap();
    assert_eq!(actual, notice);
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
        assert_eq!(hooks_override(Path::new(path), false), None, "{path}");
        assert_eq!(hooks_override(Path::new(path), true), None, "{path}");
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
    assert_eq!(hooks_override(exe, false).unwrap(), expected);
    // Byte for byte the value `hooks/list` of codex-cli 0.166.0 reported all three hooks of as
    // `trusted` on the Mac (2026-11-05).
    let notice = "hooks={Stop=[{hooks=[{type='command',command=\"'/Users/jyh/.local/bin/tabcli' native-hook codex\",timeout=30,async=false}]}],UserPromptSubmit=[{hooks=[{type='command',command=\"'/Users/jyh/.local/bin/tabcli' native-hook codex\",timeout=30,async=false}]}],PreToolUse=[{matcher='request_user_input_async',hooks=[{type='command',command=\"'/Users/jyh/.local/bin/tabcli' native-hook codex\",timeout=30,async=false}]}],state={'/<session-flags>/config.toml:stop:0:0'={trusted_hash='sha256:9a3d2ddbafef699ee6addaf9ec24e5c00cf4beaec6af55e0cae219bf28273efe'},'/<session-flags>/config.toml:user_prompt_submit:0:0'={trusted_hash='sha256:e34dae680a24bd17287545820261c0a98b9118ca447ddabc23e9cb12f0226d45'},'/<session-flags>/config.toml:pre_tool_use:0:0'={trusted_hash='sha256:e8f7d8cc0bdc38499f9bcad8d3039fb23ddbf70ef78121f3a86a045f83e15254'}}}";
    assert_eq!(hooks_override(exe, true).unwrap(), notice);
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
    const BELOW: Option<&str> = Some("version_below_0_166");
    const NO_LIFECYCLE: Option<&str> = Some("lifecycle_hooks_not_injected");
    // (version, lifecycle setting, notice setting, executable, lifecycle reason, notice reason)
    for (version, setting, notice_setting, exe, reason, notice_reason) in [
        (
            Some("codex-cli 0.147.0"),
            "on",
            None,
            available,
            Some("version_below_0_163"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.148.0"),
            "on",
            None,
            available,
            Some("version_below_0_163"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.149.0"),
            "on",
            None,
            available,
            Some("version_below_0_163"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.162.9"),
            "on",
            None,
            available,
            Some("version_below_0_163"),
            NO_LIFECYCLE,
        ),
        (Some("0.163.0"), "on", None, available, None, BELOW),
        (
            Some("0.163.0-alpha.1"),
            "on",
            None,
            available,
            Some("version_below_0_163"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.163.1-alpha.1"),
            "on",
            None,
            available,
            Some("prerelease"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.164.0-alpha.1"),
            "on",
            None,
            available,
            Some("prerelease"),
            NO_LIFECYCLE,
        ),
        (Some("0.163.0+build.7"), "on", None, available, None, BELOW),
        (
            Some("codex-cli 0.164.0"),
            "on",
            Some("on"),
            available,
            None,
            BELOW,
        ),
        (
            Some("codex-cli 0.165.9"),
            "on",
            Some("on"),
            available,
            None,
            BELOW,
        ),
        // The notice hook is opt-in: the setting's default is off, and with it on the floor is
        // 0.166.0 and newer.
        (
            Some("codex-cli 0.166.0"),
            "on",
            None,
            available,
            None,
            Some("settings_off"),
        ),
        (
            Some("codex-cli 0.170.3"),
            "on",
            None,
            available,
            None,
            Some("settings_off"),
        ),
        (
            Some("codex-cli 0.166.0"),
            "on",
            Some("on"),
            available,
            None,
            None,
        ),
        (
            Some("0.166.0+build.1"),
            "on",
            Some("on"),
            available,
            None,
            None,
        ),
        (
            Some("codex-cli 0.170.3"),
            "on",
            Some("on"),
            available,
            None,
            None,
        ),
        (
            Some("codex-cli 0.166.0"),
            "on",
            Some("off"),
            available,
            None,
            Some("settings_off"),
        ),
        (
            Some("0.166.0-alpha.1"),
            "on",
            None,
            available,
            Some("prerelease"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.167.0-alpha.1"),
            "on",
            None,
            available,
            Some("prerelease"),
            NO_LIFECYCLE,
        ),
        // Lifecycle hooks off or unavailable carry no notice hook, whatever its setting.
        (
            Some("codex-cli 0.166.0"),
            "off",
            Some("on"),
            available,
            Some("settings_off"),
            NO_LIFECYCLE,
        ),
        (
            Some("codex-cli 0.166.0"),
            "on",
            Some("on"),
            unavailable,
            Some("launcher_unavailable"),
            Some("launcher_unavailable"),
        ),
        (
            Some("unparseable"),
            "on",
            None,
            available,
            Some("version_unknown"),
            NO_LIFECYCLE,
        ),
        (
            None,
            "on",
            None,
            available,
            Some("version_unknown"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.163.0"),
            "off",
            None,
            available,
            Some("settings_off"),
            NO_LIFECYCLE,
        ),
        (
            Some("0.163.0"),
            "on",
            None,
            unavailable,
            Some("launcher_unavailable"),
            Some("launcher_unavailable"),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = test_support::launch_session(root.path(), version);
        let mut settings = serde_json::json!({"schema":1,"codex_lifecycle_hooks":setting});
        if let Some(notice_setting) = notice_setting {
            settings["codex_question_notice"] = serde_json::json!(notice_setting);
        }
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(&settings)
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
                OsString::from(hooks_override(executable, notice_reason.is_none()).unwrap()),
            ]);
        }
        if !cfg!(windows) {
            expected.extend([
                OsString::from("-C"),
                root.path().as_os_str().to_owned(),
                OsString::from(codex::correlated_prompt(prompt, &pending)),
            ]);
        }
        let case = format!("{version:?} {setting} {notice_setting:?} {exe}");
        assert_eq!(plan.arguments, expected, "{case}");
        assert_eq!(
            plan.arguments
                .iter()
                .any(|argument| argument.to_string_lossy().contains("PreToolUse")),
            notice_reason.is_none(),
            "{case}"
        );
        let mode = read_launch_mode(&directory).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(mode).unwrap(),
            serde_json::json!({
                "schema":1, "lifecycle_hooks":if reason.is_none() {"injected"} else {"not_injected"},
                "reason":reason, "codex_version":version, "vector_codex_version":"0.163.0"
            }),
            "{case}"
        );
        let notice = read_notice_mode(&directory).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(notice).unwrap(),
            serde_json::json!({
                "schema":1,
                "notice_hook":if notice_reason.is_none() {"injected"} else {"not_injected"},
                "reason":notice_reason, "vector_codex_version":"0.166.0"
            }),
            "{case}"
        );
        assert_eq!(
            notice_hook_injected(&directory).unwrap(),
            notice_reason.is_none(),
            "{case}"
        );
        // A later settings change cannot rewrite a launch's recorded decisions.
        let before = std::fs::read(directory.join(FILE)).unwrap();
        let notice_before = std::fs::read(directory.join(NOTICE_FILE)).unwrap();
        Store::open_unchecked(root.path())
            .private("settings.json")
            .write_json(
                &serde_json::json!({"schema":1,"codex_lifecycle_hooks":"off",
                "codex_question_notice": if notice_reason.is_none() {"off"} else {"on"}}),
            )
            .unwrap();
        assert!(read_launch_mode(&directory).unwrap().is_some());
        assert_eq!(std::fs::read(directory.join(FILE)).unwrap(), before);
        assert_eq!(
            notice_hook_injected(&directory).unwrap(),
            notice_reason.is_none(),
            "{case}"
        );
        assert_eq!(
            std::fs::read(directory.join(NOTICE_FILE)).unwrap(),
            notice_before
        );
    }
}

#[test]
fn an_unreadable_notice_setting_fails_the_launch_before_any_value_is_returned() {
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
    Store::open_unchecked(root.path())
        .private("settings.json")
        .write_json(&serde_json::json!({"schema":1,"codex_question_notice":"sometimes"}))
        .unwrap();
    assert!(
        prepare(
            &directory,
            Path::new(if cfg!(windows) {
                WINDOWS_EXE
            } else {
                "/opt/tabcli"
            })
        )
        .is_err()
    );
    assert!(read_notice_mode(&directory).unwrap().is_none());
}

#[test]
fn notice_mode_reader_rejects_invalid_schema_values_and_inconsistent_reason() {
    let temp = tempfile::tempdir().unwrap();
    // A session launched by an earlier binary has no record: no notice hook was injected.
    assert_eq!(read_notice_mode(temp.path()).unwrap(), None);
    assert!(!notice_hook_injected(temp.path()).unwrap());
    let valid = serde_json::json!({"schema":1,"notice_hook":"injected","reason":null,
        "vector_codex_version":"0.166.0"});
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("notice_hook", serde_json::json!("maybe")),
        ("reason", serde_json::json!("settings_off")),
        ("reason", serde_json::json!("version_below_0_163")),
        ("vector_codex_version", serde_json::json!("0.163.0")),
        ("vector_codex_version", serde_json::json!(166)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        Store::open_unchecked(temp.path())
            .private(NOTICE_FILE)
            .write_json(&invalid)
            .unwrap();
        assert!(read_notice_mode(temp.path()).is_err(), "{invalid}");
        assert!(notice_hook_injected(temp.path()).is_err(), "{invalid}");
    }
    for reason in [
        "lifecycle_hooks_not_injected",
        "version_below_0_166",
        "settings_off",
    ] {
        let off = serde_json::json!({"schema":1,"notice_hook":"not_injected","reason":reason,
            "vector_codex_version":"0.166.0"});
        Store::open_unchecked(temp.path())
            .private(NOTICE_FILE)
            .write_json(&off)
            .unwrap();
        let read = read_notice_mode(temp.path()).unwrap().unwrap();
        assert_eq!(serde_json::to_value(read).unwrap(), off);
        assert!(!notice_hook_injected(temp.path()).unwrap());
    }
    let off_without_reason = serde_json::json!({"schema":1,"notice_hook":"not_injected",
        "reason":null,"vector_codex_version":"0.166.0"});
    Store::open_unchecked(temp.path())
        .private(NOTICE_FILE)
        .write_json(&off_without_reason)
        .unwrap();
    assert!(read_notice_mode(temp.path()).is_err());
    Store::open_unchecked(temp.path())
        .private(NOTICE_FILE)
        .write_json(&valid)
        .unwrap();
    assert!(notice_hook_injected(temp.path()).unwrap());
    std::fs::write(temp.path().join(NOTICE_FILE), b"{").unwrap();
    assert!(read_notice_mode(temp.path()).is_err());
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

// The notice record is the second of the two launch records `prepare` writes. Each atomic
// record write passes four fault boundaries (temporary file, content, rename, sync after the
// rename), so a budget of `NOTICE_WRITE_START + 2` fails the notice record before its rename and
// `NOTICE_WRITE_START + 3` after it.
const NOTICE_WRITE_START: usize = 4;
// The pending-turn record precedes both launch records in the adapter's launch plan.
const PLAN_NOTICE_WRITE_START: usize = 4 + NOTICE_WRITE_START;

fn notice_payload() -> serde_json::Value {
    serde_json::json!({"session_id":"01234567-89ab-cdef-0123-456789abcdef","turn_id":"turn-1",
        "transcript_path":"rollout.jsonl","cwd":"workspace","hook_event_name":"PreToolUse",
        "model":"model","permission_mode":"bypassPermissions",
        "tool_name":"request_user_input_async",
        "tool_input":{"questions":[{"title":"Which codename?","options":["ALPHA","BRAVO"]}]},
        "tool_use_id":"call_partial"})
}

fn session_files(directory: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.extend(session_files(&path));
        } else {
            entries.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
    entries.sort();
    entries
}

// The notice is opt-in. The launches below are ones that would carry the hook.
fn turn_the_notice_on(root: &Path) {
    Store::open_unchecked(root)
        .private("settings.json")
        .write_json(&serde_json::json!({"schema":1,"codex_question_notice":"on"}))
        .unwrap();
}

fn launch_executable() -> &'static Path {
    Path::new(if cfg!(windows) {
        WINDOWS_EXE
    } else {
        "/opt/tabcli"
    })
}

// What each read-only path says about a session whose preparation failed while it wrote the
// notice record, and that none of them writes: the record alone decides, as for any launch.
fn assert_partial_notice_state(directory: &Path, present: bool, case: &str) {
    use crate::native::provider::NativeProviderAdapter;
    use crate::native::provider::codex::{ADAPTER, question_notice};
    // The lifecycle record was published before the notice record was attempted.
    assert_eq!(
        read_launch_mode(directory)
            .unwrap()
            .unwrap()
            .lifecycle_hooks,
        LifecycleHooks::Injected,
        "{case}"
    );
    assert_eq!(directory.join(NOTICE_FILE).exists(), present, "{case}");
    let before = session_files(directory);
    let mode = read_notice_mode(directory).unwrap();
    assert_eq!(
        mode.map(|mode| (mode.notice_hook, mode.reason)),
        present.then_some((NoticeHook::Injected, None)),
        "{case}"
    );
    assert_eq!(notice_hook_injected(directory).unwrap(), present, "{case}");
    let reader = Reader::open_unchecked(directory);
    assert_eq!(
        ADAPTER.question_capability(&reader).response,
        if present {
            crate::native::provider::QuestionResponse::AnswerAfterTurn("hook_configured_unverified")
        } else {
            crate::native::provider::QuestionResponse::Unsupported(
                crate::native::provider::NO_ADDRESSED_ANSWER_PATH,
            )
        },
        "{case}"
    );
    let inspected = crate::native::query::inspect_value(&reader, "session-hooks").unwrap();
    assert_eq!(
        inspected["capabilities"]["questions"]["response"]["state"],
        if present {
            "answer_after_turn"
        } else {
            "unsupported"
        },
        "{case}"
    );
    let output = question_notice::pre_tool_use(directory, &notice_payload()).unwrap();
    assert_eq!(output.is_some(), present, "{case}");
    if let Some(output) = output {
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            question_notice::NOTICE
        );
    }
    if !present {
        // The dispatch prints nothing here; with the record it prints the line checked above,
        // straight to the process's stdout.
        ADAPTER.handle_hook(directory, &notice_payload()).unwrap();
    }
    assert_eq!(
        session_files(directory),
        before,
        "{case}: a read-only path wrote"
    );
}

#[test]
fn a_notice_record_that_fails_before_or_after_its_rename_fails_preparation_without_a_value() {
    use crate::native::session::primitives::{injected_fault, with_fault_budget};
    for (passed, present, boundary) in [
        (2, false, "renaming a temporary record over its final path"),
        (3, true, "syncing a renamed record"),
    ] {
        let root = tempfile::tempdir().unwrap();
        turn_the_notice_on(root.path());
        let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
        crate::native::session::update_status(
            &directory,
            crate::native::session::SessionState::Launching,
            None,
            None,
        )
        .unwrap();
        let error = with_fault_budget(NOTICE_WRITE_START + passed, || {
            prepare(&directory, launch_executable())
        })
        .expect_err("a failed notice record must fail preparation and return no hooks value");
        assert!(injected_fault(&error), "{error:#}");
        assert!(format!("{error:#}").contains(boundary), "{error:#}");
        // A fault after the temporary file exists leaves it beside the records; no reader
        // takes it for a record.
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".agent-bridge-")
            }),
            !present,
            "{boundary}"
        );
        assert_partial_notice_state(&directory, present, boundary);
    }
}

#[test]
fn the_launch_plan_propagates_a_failed_notice_record_instead_of_returning_arguments() {
    use crate::native::session::primitives::{injected_fault, with_fault_budget};
    for (passed, present) in [(2, false), (3, true)] {
        let root = tempfile::tempdir().unwrap();
        turn_the_notice_on(root.path());
        let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
        crate::native::session::update_status(
            &directory,
            crate::native::session::SessionState::Launching,
            None,
            None,
        )
        .unwrap();
        let prompt = "[Agent Bridge native delegation]\nSource: test\n\nhello";
        // The dispatch the launch wrapper calls; a plan is the only way to the provider command.
        let result = with_fault_budget(PLAN_NOTICE_WRITE_START + passed, || {
            crate::native::provider::prepare_launch(
                agent_bridge::FirstPartyCli::Codex,
                LaunchContext {
                    bridge_executable: launch_executable(),
                    directory: &directory,
                    workspace: root.path(),
                    title: "test",
                    prompt,
                    yolo: false,
                    model: None,
                    effort: None,
                },
            )
        });
        let Err(error) = result else {
            panic!("a failed notice record returned a launch plan ({present})");
        };
        assert!(injected_fault(&error), "{error:#}");
        // The pending turn was installed before the launch records, as on every launch.
        assert!(codex::read_pending_turn(&directory).unwrap().is_some());
        assert_partial_notice_state(&directory, present, &format!("plan {present}"));
    }
}

// The launch wrapper itself: a preparation that fails on the notice record returns before the
// provider process exists. The fault budget fails every later write as well, so the wrapper would
// stop before its spawn even if the notice write were ignored; a directory at the record's path
// fails that one rename and lets every other write succeed. The fake Codex answers its version
// probe and otherwise records that it was started.
#[cfg(unix)]
#[test]
fn a_failed_notice_record_stops_the_launch_before_the_provider_is_spawned() {
    use crate::native::provider::NativeProviderAdapter;
    use crate::native::provider::codex::{ADAPTER, question_notice};
    use crate::native::session::launch::wrapper::provision;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let provider = root.path().join("fake-codex");
    let started = root.path().join("provider-started");
    std::fs::write(
        &provider,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'codex-cli 0.166.0\\n'; exit 0; fi\n: > '{}'\nexit 0\n",
            started.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    turn_the_notice_on(root.path());
    let directory = test_support::launch_session(root.path(), Some("codex-cli 0.166.0"));
    Store::open_unchecked(&directory)
        .private("manifest.json")
        .write_json(&serde_json::json!({
            "schema": 1, "id": "session-hooks", "provider": "codex",
            "provider_path": provider, "provider_version": "codex-cli 0.166.0",
            "workspace": root.path(), "title": "hooks fixture", "yolo": false,
            "created_unix_ms": 1
        }))
        .unwrap();
    crate::native::session::primitives::write_private(
        &directory.join("initial-prompt.txt"),
        b"hello",
    )
    .unwrap();
    crate::native::session::update_status(
        &directory,
        crate::native::session::SessionState::Launching,
        None,
        None,
    )
    .unwrap();
    std::fs::create_dir(directory.join(NOTICE_FILE)).unwrap();
    let error = provision(&directory).expect_err("a failed notice record must fail the launch");
    assert!(
        format!("{error:#}").contains("codex-question-notice.json"),
        "{error:#}"
    );
    assert!(
        !started.exists(),
        "the provider was spawned after a failed preparation"
    );
    assert!(!directory.join("provider-process.json").exists());
    assert_eq!(
        read_launch_mode(&directory)
            .unwrap()
            .unwrap()
            .lifecycle_hooks,
        LifecycleHooks::Injected
    );
    // No reader takes the obstruction for a decision: the record is unreadable, the capability
    // is unknown, and the hook fails (exit 1, never 2), so Codex would run the tool without the
    // notice. None of them writes.
    let before = session_files(&directory);
    assert!(read_notice_mode(&directory).is_err());
    assert!(notice_hook_injected(&directory).is_err());
    let reader = Reader::open_unchecked(&directory);
    assert!(matches!(
        ADAPTER.question_capability(&reader).response,
        crate::native::provider::QuestionResponse::Unknown(_)
    ));
    let inspected = crate::native::query::inspect_value(&reader, "session-hooks").unwrap();
    assert_eq!(
        inspected["capabilities"]["questions"]["response"]["state"],
        "unknown"
    );
    assert!(question_notice::pre_tool_use(&directory, &notice_payload()).is_err());
    assert!(ADAPTER.handle_hook(&directory, &notice_payload()).is_err());
    assert_eq!(session_files(&directory), before, "a read-only path wrote");
}
