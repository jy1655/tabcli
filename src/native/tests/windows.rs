use super::*;

#[cfg(windows)]
#[test]
fn windows_provider_resolution_accepts_exe_suffix() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("agy.exe"), b"stub").unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert_eq!(
        resolve_provider_from_path(FirstPartyCli::Agy, &path).unwrap(),
        directory.path().join("agy.exe").canonicalize().unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_provider_resolution_prefers_powershell_npm_shims_over_batch() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("codex"), b"#!/bin/sh\n").unwrap();
    fs::write(directory.path().join("codex.cmd"), b"@echo off\r\n").unwrap();
    fs::write(directory.path().join("codex.ps1"), b"#!/usr/bin/env pwsh\n").unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert_eq!(
        resolve_provider_from_path(FirstPartyCli::Codex, &path).unwrap(),
        directory.path().join("codex.ps1").canonicalize().unwrap()
    );
}

#[cfg(windows)]
#[test]
fn windows_provider_version_check_runs_cmd_shims() {
    let directory = tempfile::tempdir().unwrap();
    let shim = directory.path().join("agy.cmd");
    fs::write(&shim, "@echo off\r\necho agy 1.1.12\r\n").unwrap();
    assert_eq!(
        check_provider_version(FirstPartyCli::Agy, &shim).unwrap(),
        "agy 1.1.12"
    );
}

#[cfg(windows)]
#[test]
fn windows_state_root_falls_back_to_userprofile_without_home() {
    assert_eq!(
        default_state_root(None, Some(std::ffi::OsStr::new(r"C:\Users\cmd-user"))).unwrap(),
        PathBuf::from(r"C:\Users\cmd-user\.agent-bridge\native-sessions")
    );
}

#[cfg(windows)]
#[test]
fn windows_batch_providers_are_launched_through_a_fixed_powershell_forwarder() {
    let directory = tempfile::tempdir().unwrap();
    let command = provider_process_command(
        Path::new(r"\\?\C:\npm\codex.cmd"),
        directory.path(),
        vec![OsString::from("--config"), OsString::from("a&b")],
    )
    .unwrap();
    assert!(Path::new(command.get_program()).is_absolute());
    assert_eq!(
        Path::new(command.get_program()).file_name().unwrap(),
        "pwsh.exe"
    );
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(args[0], "-NoLogo");
    assert_eq!(args[1], "-NoProfile");
    assert_eq!(args[2], "-ExecutionPolicy");
    assert_eq!(args[3], "Bypass");
    assert_eq!(args[4], "-File");
    assert_eq!(args[6], r"C:\npm\codex.cmd");
    assert_eq!(args[8], "a&b");
}

#[cfg(windows)]
#[test]
fn windows_batch_providers_preserve_percent_and_shell_metacharacters() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider.cmd");
    write_private(
        &provider,
        br#"@echo off
set "AGENT_BRIDGE_PROBE_1=%~1"
set "AGENT_BRIDGE_PROBE_2=%~2"
pwsh.exe -NoLogo -NoProfile -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::WriteLine('ARG=[' + $env:AGENT_BRIDGE_PROBE_1 + ']'); [Console]::WriteLine('ARG=[' + $env:AGENT_BRIDGE_PROBE_2 + ']')"
"#,
    )
    .unwrap();
    let arguments = vec![
        OsString::from("prompt %SECRET_ENV% 100%"),
        OsString::from("owner's & | < > ^ !"),
    ];
    provider_process_command(&provider, directory.path(), arguments.clone()).unwrap();
    let mut command = provider_process_command(&provider, directory.path(), arguments).unwrap();
    command.env("SECRET_ENV", "EXPANDED");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "ARG=[prompt %SECRET_ENV% 100%]",
            "ARG=[owner's & | < > ^ !]",
        ]
    );
}

#[cfg(windows)]
#[test]
fn windows_console_helper_accepts_only_managed_session_identity() {
    assert!(matches!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "pending-prompt-1.txt",
            "5000",
        ])
        .unwrap(),
        NativeCommand::ConsoleControl {
            request: ConsoleControlRequest::Send { input_name, timeout },
            id,
        } if timeout == Duration::from_millis(5000) && id == "session-owner123" && input_name == "pending-prompt-1.txt"
    ));
    assert!(parse_args(["native-console-control", "close", "1234"]).is_err());
    assert!(
        parse_args([
            "native-console-control",
            "send",
            "session-owner123",
            "..\\outside.txt",
            "5000",
        ])
        .is_err()
    );
    // The window lookup is bounded and takes nothing but its time.
    assert!(matches!(
        parse_args(["native-console-control", "window", "session-owner123", "600"]).unwrap(),
        NativeCommand::ConsoleControl {
            request: ConsoleControlRequest::Window { timeout },
            id,
        } if timeout == Duration::from_millis(600) && id == "session-owner123"
    ));
    for invalid in [
        &["window", "session-owner123"][..],
        &["window", "session-owner123", "0"],
        &["window", "session-owner123", "pending-prompt-1.txt", "600"],
    ] {
        let mut arguments = vec!["native-console-control"];
        arguments.extend_from_slice(invalid);
        assert!(parse_args(arguments).is_err(), "{invalid:?}");
    }
}

#[cfg(windows)]
#[test]
fn windows_terminal_input_requires_the_live_native_session_owner() {
    let directory = tempfile::tempdir().unwrap();
    let owner = current_native_session_owner("session-owner123").unwrap();
    write_json_atomic(&directory.path().join(SESSION_OWNER_FILE), &owner).unwrap();

    verified_windows_native_owner(directory.path(), "session-owner123").unwrap();
    assert!(
        verified_windows_native_owner(directory.path(), "session-other123")
            .unwrap_err()
            .to_string()
            .contains("not bound")
    );
}

#[cfg(windows)]
#[test]
fn windows_close_control_uses_the_claimed_terminal_handle() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(TERMINAL_HANDLE_FILE), b"active").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), &ConsoleControlRequest::Close),
        directory.path().join(TERMINAL_HANDLE_FILE)
    );
    fs::write(directory.path().join(TERMINAL_CLOSING_FILE), b"closing").unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), &ConsoleControlRequest::Close),
        directory.path().join(TERMINAL_CLOSING_FILE)
    );
}

#[cfg(windows)]
#[test]
fn powershell_launch_quoting_doubles_apostrophes() {
    let command = bridge_shell_command(
        Path::new("C:\\work\\owner's repo"),
        Path::new("C:\\state"),
        Path::new("C:\\bin\\agent-bridge.exe"),
        "session-safe123",
    )
    .unwrap();
    assert!(command.contains("'C:\\work\\owner''s repo'"));
    assert!(!command.contains("owner's repo"));
}

// Windows refuses to replace a file while any other handle to it is open. A launcher
// polls `status.json` and `launch.json` while the wrapper replaces them, so each
// replacement can meet a reader that holds the record for a moment.
#[cfg(windows)]
#[test]
fn a_record_is_replaced_although_a_reader_holds_it_open_for_a_moment() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("status.json");
    write_json_atomic(&path, &serde_json::json!({ "state": "launching" })).unwrap();
    let reader = File::open(&path).unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        drop(reader);
    });

    write_json_atomic(&path, &serde_json::json!({ "state": "running" })).unwrap();

    release.join().unwrap();
    assert_eq!(
        read_json::<serde_json::Value>(&path).unwrap()["state"],
        "running"
    );
}

#[cfg(windows)]
#[test]
fn a_record_that_stays_open_fails_the_replacement_after_a_bounded_wait() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("status.json");
    write_json_atomic(&path, &serde_json::json!({ "state": "launching" })).unwrap();
    let reader = File::open(&path).unwrap();
    let started = Instant::now();

    let error = write_json_atomic(&path, &serde_json::json!({ "state": "running" })).unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(format!("{error:#}").contains("failed to persist"));
    drop(reader);
    assert_eq!(
        read_json::<serde_json::Value>(&path).unwrap()["state"],
        "launching"
    );
    // The temporary record is removed, not left beside the one it could not replace.
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn console_control_requests_preserve_surface_verification_and_handle_selection() {
    let directory = tempfile::tempdir().unwrap();
    let closing = directory.path().join(TERMINAL_CLOSING_FILE);
    fs::create_dir(&closing).unwrap();
    assert_eq!(
        windows_console_handle_path(directory.path(), &ConsoleControlRequest::Close),
        directory.path().join(TERMINAL_HANDLE_FILE)
    );
    fs::remove_dir(&closing).unwrap();
    fs::write(&closing, b"closing").unwrap();
    for (request, bound_only) in [
        (ConsoleControlRequest::Screen, false),
        (
            ConsoleControlRequest::Send {
                input_name: "pending-prompt-route.txt".into(),
                timeout: Duration::from_millis(7),
            },
            false,
        ),
        (
            ConsoleControlRequest::Dialog {
                input_name: "pending-prompt-route.txt".into(),
                timeout: Duration::from_millis(7),
            },
            false,
        ),
        (
            ConsoleControlRequest::Window {
                timeout: Duration::from_millis(7),
            },
            true,
        ),
        (ConsoleControlRequest::Close, true),
    ] {
        assert_eq!(request.needs_only_bound_surface(), bound_only);
        let expected = if request == ConsoleControlRequest::Close {
            TERMINAL_CLOSING_FILE
        } else {
            TERMINAL_HANDLE_FILE
        };
        assert_eq!(
            windows_console_handle_path(directory.path(), &request),
            directory.path().join(expected)
        );
    }
}

// Each stage runs in a test child: never mutate the test runner's environment.
#[cfg(windows)]
#[test]
fn windows_environment_boundary_replaces_caller_context_through_bootstrap_and_hook() {
    use crate::native::session::primitives::{state_root, validate_hook_directory};
    use std::os::windows::process::CommandExt;
    const STAGE: &str = "ISSUE111_TEST_STAGE";
    const ROOT: &str = "ISSUE111_TEST_ROOT";
    const ID: &str = "session-environment";
    let test = "native::tests::windows::windows_environment_boundary_replaces_caller_context_through_bootstrap_and_hook";
    let executable = std::env::current_exe().unwrap();
    let run = |command: &mut Command| {
        let output = process_env::command_output_until(
            command,
            Instant::now() + Duration::from_secs(30),
            "environment boundary test",
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let stage = std::env::var(STAGE).unwrap_or_default();
    if stage.is_empty() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(ID)).unwrap();
        let mut child = Command::new(&executable);
        child
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(STAGE, "launcher")
            .env(ROOT, root.path());
        for name in process_env::non_provider_removals() {
            child.env(name, "caller-sentinel");
        }
        run(&mut child);
        assert_eq!(
            fs::read(root.path().join("hook-verified")).unwrap(),
            b"verified"
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let directory = root.join(ID);
    if stage == "launcher" {
        for name in process_env::non_provider_removals() {
            assert_eq!(std::env::var(name).unwrap(), "caller-sentinel");
        }
        let stub = root.join("bridge owner's stub.cmd");
        fs::write(&stub, format!(
            "@echo off\r\n\"{}\" --exact {test} --nocapture --test-threads=1\r\n@exit /b %errorlevel%\r\n",
            executable.display(),
        )).unwrap();
        let bootstrap = bridge_shell_command(&root, &root, &stub, ID).unwrap();
        let powershell = terminal::windows_powershell_executable().unwrap();
        let line = terminal::windows_console_launch_command_line(&powershell, &bootstrap).unwrap();
        let arguments = line
            .strip_prefix(&format!("\"{}\" ", powershell.display()))
            .unwrap();
        let mut command = process_env::helper_command(&powershell);
        command.raw_arg(arguments).env(STAGE, "wrapper");
        run(&mut command);
        return;
    }
    assert_eq!(state_root().unwrap(), root);
    for name in process_env::non_provider_removals() {
        if name != STATE_DIR_ENV
            && (stage == "wrapper"
                || ![
                    SESSION_DIR_ENV,
                    "AGENT_BRIDGE_NATIVE_SESSION_ID",
                    "AGENT_BRIDGE_EXECUTABLE",
                ]
                .contains(&name))
        {
            assert!(std::env::var_os(name).is_none(), "{name}");
        }
    }
    if stage == "wrapper" {
        let mut command = process_env::provider_command(
            &executable,
            &directory,
            ["--exact", test, "--nocapture", "--test-threads=1"]
                .map(OsString::from)
                .to_vec(),
            provider::probe_environment_removals(FirstPartyCli::Codex),
        )
        .unwrap();
        // Prove the launch configuration replaces these values, rather than relying
        // only on their absence at the clean wrapper boundary.
        for name in [
            SESSION_DIR_ENV,
            "AGENT_BRIDGE_NATIVE_SESSION_ID",
            "AGENT_BRIDGE_EXECUTABLE",
        ] {
            command.env(name, "caller-sentinel");
        }
        configure_provider_launch(&mut command, &root, &directory, ID, &executable);
        command.env(STAGE, "provider");
        run(&mut command);
    } else {
        assert_eq!(
            std::env::var_os(SESSION_DIR_ENV).unwrap(),
            directory.as_os_str()
        );
        assert_eq!(std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID").unwrap(), ID);
        assert_eq!(
            std::env::var_os("AGENT_BRIDGE_EXECUTABLE").unwrap(),
            executable.as_os_str()
        );
        validate_hook_directory(&directory).unwrap();
        if stage == "provider" {
            // A provider-owned hook inherits the new session's environment.
            let mut command = Command::new(&executable);
            command
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(STAGE, "hook");
            run(&mut command);
        } else {
            assert_eq!(stage, "hook");
            fs::write(root.join("hook-verified"), b"verified").unwrap();
        }
    }
}
