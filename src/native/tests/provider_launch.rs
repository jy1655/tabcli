use super::*;
use crate::native::session::launch::launcher::bridge_shell_command;
use crate::native::session::launch::wrapper::{
    STDERR_ENV, STDOUT_ENV, configure_provider_launch, provider_version_text, version_probe_command,
};
#[cfg(unix)]
use crate::native::session::launch::wrapper::{
    check_provider_version, check_provider_version_until, provision, resolve_provider_from_path,
};
#[cfg(unix)]
use crate::native::session::primitives::{read_json, write_json_atomic, write_private};
#[cfg(unix)]
use crate::native::session::test_support::PROVIDER_PROCESS_FILE;
#[cfg(unix)]
use crate::native::session::{SessionState, SessionStatus, launch, update_status};
use crate::native::{native_delegation_prompt, process_env, provider, sanitize_title};
#[cfg(unix)]
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
#[test]
fn shell_quoting_handles_apostrophes_without_executing_them() {
    assert_eq!(
        shell_quote(std::ffi::OsStr::new("/tmp/user's bridge")),
        "'/tmp/user'\"'\"'s bridge'"
    );
}

#[test]
fn bridge_shell_command_quotes_the_workspace_and_executable() {
    let command = bridge_shell_command(
        Path::new("/tmp/project; touch nope"),
        Path::new("/tmp/state root"),
        Path::new("/tmp/Agent Bridge/bin"),
        "session-safe123",
    )
    .unwrap();
    #[cfg(unix)]
    assert!(command.contains("cd '/tmp/project; touch nope' && AGENT_BRIDGE_LAUNCH_STDERR_FD=3 AGENT_BRIDGE_LAUNCH_STDOUT_FD=4 AGENT_BRIDGE_NATIVE_STATE_DIR='/tmp/state root' '/tmp/Agent Bridge/bin' native-session 'session-safe123'"));
    #[cfg(windows)]
    assert!(command.contains("Set-Location -LiteralPath '/tmp/project; touch nope' -ErrorAction Stop; $env:AGENT_BRIDGE_NATIVE_STATE_DIR = '/tmp/state root'; & '/tmp/Agent Bridge/bin' native-session 'session-safe123'"));
}

#[test]
fn managed_shell_exits_after_the_native_session_instead_of_accepting_late_input() {
    let command = bridge_shell_command(
        Path::new("/tmp/workspace"),
        Path::new("/tmp/state"),
        Path::new("/tmp/bridge"),
        "session-safe123",
    )
    .unwrap();

    #[cfg(unix)]
    assert!(command.ends_with("exit \"$bridge_status\""));
    #[cfg(windows)]
    assert!(command.ends_with("exit $bridgeStatus"));
}

#[test]
fn bridge_shell_command_rejects_controls_in_dynamic_components() {
    for control in ['\0', '\n', '\t', '\r', '\u{1b}', '\u{7f}'] {
        let unsafe_value = format!("unsafe{control}value");
        for result in [
            bridge_shell_command(
                Path::new(&unsafe_value),
                Path::new("/tmp/state"),
                Path::new("/tmp/bridge"),
                "session-safe123",
            ),
            bridge_shell_command(
                Path::new("/tmp/workspace"),
                Path::new(&unsafe_value),
                Path::new("/tmp/bridge"),
                "session-safe123",
            ),
            bridge_shell_command(
                Path::new("/tmp/workspace"),
                Path::new("/tmp/state"),
                Path::new(&unsafe_value),
                "session-safe123",
            ),
            bridge_shell_command(
                Path::new("/tmp/workspace"),
                Path::new("/tmp/state"),
                Path::new("/tmp/bridge"),
                &unsafe_value,
            ),
        ] {
            assert!(
                result.is_err(),
                "accepted terminal control U+{:04X}",
                u32::from(control)
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn provider_resolution_rejects_relative_path_entries() {
    use std::os::unix::fs::PermissionsExt;

    let cwd = std::env::current_dir().unwrap();
    let root = tempfile::Builder::new()
        .prefix("relative-provider-")
        .tempdir_in(&cwd)
        .unwrap();
    let relative = root.path().strip_prefix(&cwd).unwrap();
    let executable = root.path().join("codex");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(resolve_provider_from_path(FirstPartyCli::Codex, relative.as_os_str()).is_err());
}

#[test]
fn every_native_prompt_carries_a_sanitized_source_provenance() {
    assert_eq!(
        native_delegation_prompt("Codex parent\nforged", "review this"),
        "[Agent Bridge native delegation]\nSource: Codex parent forged\n\nreview this"
    );
}

#[test]
fn session_metadata_titles_drop_control_characters_and_are_bounded() {
    assert_eq!(sanitize_title(" Review\nTab\t ").unwrap(), "Review Tab");
    assert_eq!(
        sanitize_title(&"x".repeat(200)).unwrap().chars().count(),
        80
    );
    assert!(sanitize_title("\n\t").is_err());
}

// The wrapper runs exactly the adapter's plan: its complete argument list, in the
// workspace, with the adapter's environment removals applied and Bridge's session
// variables set. Claude's removal list is non-empty, so a dropped list is observable; an
// inherited CLAUDE_CODE_CHILD_SESSION is the marker that would stop the managed session
// from registering its inbox (issue #42). The spawned process is recorded, the accepted
// prompt is removed and the session runs before the provider exits, and the exit settles.
#[cfg(unix)]
#[test]
fn native_session_runs_the_adapters_complete_plan_with_its_environment() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let provider = root.path().join("fake-claude");
    fs::write(
        &provider,
        concat!(
            "#!/bin/sh\n",
            "if [ \"$1\" = \"--version\" ]; then printf '2.1.234 (Claude Code)\\n'; exit 0; fi\n",
            "printf '%s\\n' \"$@\" > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/argv.txt\"\n",
            "pwd > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/cwd.txt\"\n",
            "env > \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/env.txt\"\n",
            "i=0\n",
            "until grep -q '\"running\"' \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/status.json\" 2>/dev/null; do\n",
            "  i=$((i + 1)); [ \"$i\" -gt 500 ] && exit 3; sleep 0.01\n",
            "done\n",
            "cp \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/status.json\" \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/status-at-run.json\"\n",
            "[ -e \"$AGENT_BRIDGE_NATIVE_SESSION_DIR/initial-prompt.txt\" ] && exit 4\n",
            "exit 0\n",
        ),
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

    let directory = root.path().join("session-safe123");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: "session-safe123".to_owned(),
            provider: "claude".to_owned(),
            provider_path: provider,
            provider_version: "2.1.234 (Claude Code)".to_owned(),
            workspace: workspace.clone(),
            title: "Claude test".to_owned(),
            model: Some("Fable5".to_owned()),
            effort: Some("high".to_owned()),
            yolo: true,
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();
    write_private(&directory.join("initial-prompt.txt"), b"claude prompt").unwrap();

    update_status(&directory, SessionState::Launching, None, None).unwrap();
    let claim = turn::claim(&Store::open_unchecked(&directory), &[]).unwrap();
    claim.retain();
    // SAFETY: the test harness runs serially (`--test-threads=1`); the marker is set for
    // this spawn only and removed before the test ends.
    unsafe { std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "inherited-from-parent") };
    let result = provision(&directory);
    unsafe { std::env::remove_var("CLAUDE_CODE_CHILD_SESSION") };
    launch::finalize(&Store::open_unchecked(&directory), &result).unwrap();
    result.unwrap();

    let arguments = fs::read_to_string(directory.join("argv.txt")).unwrap();
    assert_eq!(
        arguments.lines().collect::<Vec<_>>(),
        vec![
            "--dangerously-skip-permissions",
            "--effort",
            "high",
            "--model",
            "Fable",
            "--settings",
            directory.join("claude-settings.json").to_str().unwrap(),
            "--name",
            "session-safe123",
            "claude prompt",
        ]
    );
    assert_eq!(
        fs::read_to_string(directory.join("cwd.txt"))
            .unwrap()
            .trim(),
        workspace.canonicalize().unwrap().to_string_lossy()
    );
    let environment = fs::read_to_string(directory.join("env.txt")).unwrap();
    let variable = |name: &str| {
        environment
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
            })
            .map(str::to_owned)
    };
    assert_eq!(variable("CLAUDE_CODE_CHILD_SESSION"), None, "{environment}");
    assert_eq!(
        variable(SESSION_DIR_ENV).as_deref(),
        Some(directory.to_str().unwrap())
    );
    assert_eq!(
        variable("AGENT_BRIDGE_NATIVE_SESSION_ID").as_deref(),
        Some("session-safe123")
    );
    assert_eq!(
        variable("AGENT_BRIDGE_EXECUTABLE").map(PathBuf::from),
        Some(std::env::current_exe().unwrap())
    );
    assert_eq!(variable(STDERR_ENV), None);
    assert_eq!(variable(STDOUT_ENV), None);
    let record: ProviderProcessRecord = read_json(&directory.join(PROVIDER_PROCESS_FILE)).unwrap();
    assert_eq!(record.schema, 1);
    assert_eq!(record.managed_session_id, "session-safe123");
    assert_ne!(record.pid, 0);
    assert!(record.spawned_unix_ms > 0);
    let at_run: SessionStatus = read_json(&directory.join("status-at-run.json")).unwrap();
    assert_eq!(at_run.state.as_str(), "running");
    assert!(!directory.join("initial-prompt.txt").exists());
    let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
    assert_eq!(status.state.as_str(), "exited");
    let log = fs::read_to_string(directory.join("launch.log")).unwrap();
    assert!(log.contains("provider_exit_code=Some(0)"), "{log}");
}

#[test]
fn provider_launch_and_version_builders_preserve_their_exact_environments() {
    for provider in agent_bridge::supported_clis() {
        let removals = provider::probe_environment_removals(*provider);
        let executable = Path::new("unused");
        let directory = Path::new(".");
        let mut command =
            process_env::provider_command(executable, directory, vec![], removals).unwrap();
        configure_provider_launch(&mut command, directory, directory, "session-e1", executable);
        process_env::tests::assert_provider_environment(
            &command,
            removals,
            &[
                (SESSION_DIR_ENV, Some(directory.as_os_str())),
                (
                    "AGENT_BRIDGE_NATIVE_SESSION_ID",
                    Some(std::ffi::OsStr::new("session-e1")),
                ),
                ("AGENT_BRIDGE_EXECUTABLE", Some(executable.as_os_str())),
                (STDERR_ENV, None),
                (STDOUT_ENV, None),
            ],
        );
        process_env::tests::assert_provider_environment(
            &version_probe_command(*provider, executable).unwrap(),
            removals,
            &[],
        );
    }
}

#[test]
fn provider_version_text_selects_trimmed_lossy_stdout_before_stderr() {
    for (stdout, stderr, expected) in [
        (
            &b" \tcodex-cli 0.147.0\r\n"[..],
            &b"different"[..],
            "codex-cli 0.147.0",
        ),
        (b"", b" \tagy 1.1.12\r\n", "agy 1.1.12"),
        (b" \t\r\n", b" agy 1.1.12 \n", "agy 1.1.12"),
        ("\u{2003}\u{00a0}\n".as_bytes(), b"agy 1.1.12", "agy 1.1.12"),
        (b" \t\n", b" \r\n", ""),
        (b"development build", b"agy 1.1.12", "development build"),
        (b"\xff", b"agy 1.1.12", "\u{fffd}"),
        (b" \t\n", b" \xff \n", "\u{fffd}"),
        (
            b"  banner\nagy 1.1.12\nextra  \n",
            b"ignored",
            "banner\nagy 1.1.12\nextra",
        ),
    ] {
        assert_eq!(provider_version_text(stdout, stderr), expected);
    }
}

#[cfg(unix)]
#[test]
fn provider_version_check_preserves_output_selection_and_failures() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("provider");
    let nonzero = format!(
        "{} --version exited with exit status: 7",
        executable.display()
    );
    for (status, stdout, stderr, expected) in [
        (0, "agy 1.1.12", "agy 1.1.11", Ok("agy 1.1.12")),
        (0, " \t\n", "agy 1.1.12", Ok("agy 1.1.12")),
        (
            0,
            "agy 1.1.11",
            "agy 1.1.12",
            Err("agy is too old: found \"agy 1.1.11\", require >= 1.1.12"),
        ),
        (
            0,
            "development build",
            "agy 1.1.12",
            Err("could not parse a semantic version from \"development build\""),
        ),
        (
            0,
            " \t\n",
            " \r\n",
            Err("could not parse a semantic version from \"\""),
        ),
        (7, "agy 1.1.12", "failed", Err(nonzero.as_str())),
    ] {
        fs::write(&executable, format!(
            "#!/bin/sh\n[ \"$#\" -eq 1 ] && [ \"$1\" = '--version' ] || exit 99\nprintf '%s' '{stdout}'\nprintf '%s' '{stderr}' >&2\nexit {status}\n"
        )).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        for bounded in [false, true] {
            let actual = if bounded {
                check_provider_version_until(
                    FirstPartyCli::Agy,
                    &executable,
                    Some(Instant::now() + Duration::from_secs(5)),
                )
            } else {
                check_provider_version(FirstPartyCli::Agy, &executable)
            }
            .map_err(|error| error.to_string());
            assert_eq!(
                actual.as_deref().map_err(String::as_str),
                expected,
                "bounded={bounded}, exit={status}, stdout={stdout:?}"
            );
        }
    }
}
