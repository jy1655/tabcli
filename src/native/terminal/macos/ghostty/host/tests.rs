use super::super::tests::{Pty, bind_fixture, existing, launch_fixture, soon};
use super::super::{CREATE_SURFACE_SCRIPT, CreatedSurface, create_with_runner, handle};
use super::*;
use crate::native::terminal::ownership;

#[test]
fn launch_context_does_not_depend_on_terminal_environment() {
    if !crate::native::process_env::tests::without_bridge_environment(
        module_path!(),
        "launch_context_does_not_depend_on_terminal_environment",
    ) {
        return;
    }

    let directory = launch_fixture();
    let surface = handle(&CreatedSurface {
        tab_id: "t2".into(),
        window_id: "w1".into(),
        terminal_id: "u2".into(),
    });
    bind_fixture(directory.path(), &surface);
    let token = crate::native::session::launch::read(&Reader::open_unchecked(directory.path()))
        .unwrap()
        .unwrap()
        .claim_token;
    let wrapper = crate::native::process_env::tests::wrapper_for(directory.path());
    launch_command(directory.path(), &wrapper, None).unwrap();
    let frame = encode(directory.path(), &token).unwrap();
    let Frame {
        token: received_token,
        directory: received_directory,
    } = parse(frame.as_bytes()).unwrap().unwrap();
    assert_eq!(received_directory, directory.path());
    let script = verify_launch(&received_directory, &received_token).unwrap();
    assert_eq!(script, directory.path().join("ghostty-start.sh"));
    assert_eq!(
        std::fs::read_to_string(script).unwrap(),
        format!("{wrapper}\n")
    );
}

#[test]
fn bound_start_preserves_invoking_path_for_env_interpreters() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("runtime with ' quotes");
    fs::create_dir(&bin).unwrap();
    let interpreter = bin.join("bridge-test-runtime");
    fs::write(&interpreter, "#!/bin/sh\nprintf 'provider-ready\\n'\n").unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    let provider = temp.path().join("provider");
    fs::write(&provider, "#!/usr/bin/env bridge-test-runtime\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let script = temp.path().join("launch.sh");
    fs::write(
        &script,
        format!(
            "{} --version",
            crate::native::terminal::shell_quote(provider.as_os_str())
        ),
    )
    .unwrap();
    let source = format!(
        ". {}",
        crate::native::terminal::shell_quote(script.as_os_str())
    );
    let run = |command: &str| {
        Command::new("/bin/zsh")
            .args(["-f", "-c", command])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    };
    assert_eq!(run(&source).status.code(), Some(127));
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run(&launch_command(temp.path(), &source, Some(&path)).unwrap());
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"provider-ready\n");
    assert_eq!(
        fs::metadata(temp.path().join("ghostty-start.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(run(&source).status.code(), Some(127));
}

#[test]
fn regression_long_path_is_not_injected_into_canonical_input_line() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("launch.sh");
    std::fs::write(&script, "true\n").unwrap();
    let source = format!(
        ". {}",
        crate::native::terminal::shell_quote(script.as_os_str())
    );
    let path = format!("/{}:/usr/bin:/bin", "p".repeat(1300));
    let command =
        launch_command(directory.path(), &source, Some(std::ffi::OsStr::new(&path))).unwrap();
    assert!(
        command.len() < 1024,
        "injected {}-byte canonical input line",
        command.len()
    );
    assert!(!command.contains(&path));
    let output = std::process::Command::new("/bin/zsh")
        .args(["-f", "-c", &format!("{command}; printf '%s' \"$PATH\"")])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, path.as_bytes());
}
// Issue #58, reproduced by hand on 2026-10-03 in iTerm2 and Terminal.app: a key typed
// while the new surface was selected arrived before the launch command, the shell
// ran `a. '/.../launch.sh'`, and the provider never started. Replayed on a private
// PTY with the process Ghostty runs for the creation command; no terminal
// application and no keyboard is involved.
#[test]
fn regression_startup_keys_cannot_change_the_launch_command() {
    let directory = launch_fixture();
    let marker = directory.path().join("started");
    let launch = directory.path().join("launch.sh");
    std::fs::write(
        &launch,
        format!(
            "printf started > {}; exit\n",
            crate::native::terminal::shell_quote(marker.as_os_str())
        ),
    )
    .unwrap();
    let mut f = existing();
    f.pty = Some(Pty {
        keys_before: b"a",
        ..Pty::default()
    });
    let handle = create_with_runner(&mut f, false, soon()).unwrap();
    bind_fixture(directory.path(), &handle);
    start(
        &mut f,
        &handle,
        &format!(
            ". {}",
            crate::native::terminal::shell_quote(launch.as_os_str())
        ),
        directory.path(),
        soon(),
    )
    .unwrap();
    let pty = f.pty.as_mut().unwrap();
    pty.run(|| marker.exists(), Duration::from_secs(5));
    assert!(
        marker.exists(),
        "a typed key changed the launch command: {}",
        pty.shown()
    );
}
const PROBE: &str = "--exact native::terminal::macos::ghostty::host::tests::ghostty_host_probe --nocapture --test-threads=1";
// Runs only as a process on the private PTY of the tests below, where the test
// executable stands in for the installed one: `host` is the creation command,
// `owner` the launch command.
#[test]
fn ghostty_host_probe() {
    let Ok(mode) = std::env::var("AB_GHOSTTY_PROBE") else {
        return;
    };
    if mode == "host" {
        // The test harness writes to standard output; the surface keeps the real
        // one, which the shell reads, on descriptor 3.
        assert_eq!(unsafe { libc::dup2(3, 1) }, 1);
        if let Err(error) = run_host() {
            eprintln!("{error:#}");
            std::process::exit(7);
        }
        std::process::exit(0);
    }
    let directory = PathBuf::from(std::env::var_os("AB_GHOSTTY_PROBE_DIR").unwrap());
    let id = directory.file_name().unwrap().to_str().unwrap();
    let owner = ownership::current_native_session_owner(id).unwrap();
    let mut queued: libc::c_int = -1;
    assert_eq!(unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) }, 0);
    crate::native::session::primitives::write_json_atomic(
        &directory.join("owner-probe.json"),
        &serde_json::json!({
            "group": owner.process_group,
            "terminal_group": owner.terminal_process_group,
            "shell": owner.terminal_shell.map(|shell| shell.pid),
            "queued_input": queued,
        }),
    )
    .unwrap();
}
#[test]
fn creation_runs_only_the_host_and_the_start_writes_one_frame_and_no_enter() {
    let directory = launch_fixture();
    let token = crate::native::session::launch::read(
        &crate::native::session::Reader::open_unchecked(directory.path()),
    )
    .unwrap()
    .unwrap()
    .claim_token;
    let mut f = existing();
    let handle = create_with_runner(&mut f, false, soon()).unwrap();
    assert_eq!(
        f.command,
        bootstrap(std::env::current_exe().unwrap().as_os_str()).unwrap()
    );
    assert!(f.command.starts_with("/bin/zsh -f -i -c 'start=$("));
    assert!(
        f.command
            .ends_with(" native-ghostty-host) || exit; . \"$start\"'")
    );
    assert!(!f.command.contains(directory.path().to_str().unwrap()));
    let shell_args = crate::native::terminal::macos::test_support::shell_argv(&f.command);
    assert_eq!(&shell_args[..4], ["/bin/zsh", "-f", "-i", "-c"]);
    let invocation = shell_args[4]
        .strip_prefix("start=$(")
        .unwrap()
        .strip_suffix(") || exit; . \"$start\"")
        .unwrap();
    let args = crate::native::terminal::macos::test_support::host_argv(invocation);
    assert_eq!(args, [HOST_COMMAND]);
    assert!(matches!(
        crate::native::parse_args(args).unwrap(),
        crate::native::NativeCommand::Host(crate::native::terminal::host::HostCommand::Ghostty)
    ));

    assert!(CREATE_SURFACE_SCRIPT.contains("set command of cfg to item 2 of argv"));
    bind_fixture(directory.path(), &handle);
    let created = f.calls.len();
    start(&mut f, &handle, ". '/launch.sh'", directory.path(), soon()).unwrap();
    assert_eq!(f.calls[created..], ["input"], "one write and no Enter");
    assert_eq!(
        f.typed,
        format!(
            "agent-bridge-ghostty-host:{token}:{}:{}",
            directory.path().as_os_str().len(),
            directory.path().display()
        )
    );
    assert!(
        std::fs::read_to_string(directory.path().join("ghostty-start.sh"))
            .unwrap()
            .ends_with("\n. '/launch.sh'\n")
    );
    // Without a launch receipt there is nothing to hand to a host.
    let stray = tempfile::tempdir().unwrap();
    assert!(start(&mut f, &handle, ". '/launch.sh'", stray.path(), soon()).is_err());
    assert_eq!(f.calls.len(), created + 1, "a frame without a launch");
}
#[test]
fn host_takes_a_frame_from_among_typed_keys_and_only_for_its_pending_bound_launch() {
    let surface = handle(&CreatedSurface {
        tab_id: "t2".into(),
        window_id: "w1".into(),
        terminal_id: "u2".into(),
    });
    let prepared = || {
        let directory = launch_fixture();
        bind_fixture(directory.path(), &surface);
        launch_command(directory.path(), ". '/launch.sh'", None).unwrap();
        let token = crate::native::session::launch::read(
            &crate::native::session::Reader::open_unchecked(directory.path()),
        )
        .unwrap()
        .unwrap()
        .claim_token;
        (directory, token)
    };
    let (directory, token) = prepared();
    let path = directory.path();
    // Keys before and after the frame are somebody else's; a frame that has not
    // arrived completely is waited for.
    let frame = encode(path, &token).unwrap();
    let mut typed = b"aaaa\nmore\x03\x15".to_vec();
    typed.extend_from_slice(frame.as_bytes());
    for cut in [10, typed.len() - frame.len() + 30, typed.len() - 1] {
        assert_eq!(parse(&typed[..cut]).unwrap(), None, "{cut}");
    }
    typed.extend_from_slice(b"zz\n");
    assert_eq!(
        parse(&typed).unwrap(),
        Some(Frame {
            token: token.clone(),
            directory: path.to_path_buf()
        })
    );
    for malformed in [
        "bad token:3:/ab",
        ":3:/ab",
        "token:0:",
        "token:x:/ab",
        "token:4096:/",
    ] {
        assert!(
            parse(format!("{HOST_FRAME}{malformed}").as_bytes()).is_err(),
            "{malformed}"
        );
    }
    assert_eq!(
        verify_launch(path, &token).unwrap(),
        path.join("ghostty-start.sh")
    );
    use crate::native::session::primitives::write_json_atomic;
    use crate::native::session::test_support::TERMINAL_HANDLE_FILE;
    use crate::native::session::{launch, update_status};
    use std::os::unix::fs::PermissionsExt;
    let (readable_directory, readable_token) = prepared();
    let script = readable_directory.path().join("ghostty-start.sh");
    assert!(std::fs::symlink_metadata(&script).unwrap().is_file());
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        verify_launch(readable_directory.path(), &readable_token)
            .unwrap_err()
            .to_string(),
        "Ghostty start script is not private to the current user"
    );
    for case in [
        "the token of another launch",
        "the receipt of another launch",
        "not bound",
        "bound to another session",
        "bound to another terminal",
        "cancelled",
        "expired",
        "already spawned",
        "claim released",
        "directory open to others",
        "no start script",
        "start script is a link",
        "relative directory",
    ] {
        let (directory, token) = prepared();
        let path = directory.path();
        let binding = path.join(TERMINAL_HANDLE_FILE);
        let mut receipt = launch::read(&crate::native::session::Reader::open_unchecked(path))
            .unwrap()
            .unwrap();
        let mut sent = (path.to_path_buf(), token.clone());
        match case {
            "the token of another launch" => sent.1 = "1-2-3".into(),
            "the receipt of another launch" => receipt.claim_token = "1-2-3".into(),
            "not bound" => std::fs::remove_file(&binding).unwrap(),
            "bound to another session" => write_json_atomic(
                &binding,
                &serde_json::json!({ "terminal": "ghostty", "session_id": "u2", "tab_id": "t2", "window_id": "w1", "managed_session_id": "session-other" }),
            )
            .unwrap(),
            "bound to another terminal" => write_json_atomic(
                &binding,
                &serde_json::json!({ "terminal": "iterm2", "session_id": "u2", "managed_session_id": path.file_name().unwrap().to_str().unwrap() }),
            )
            .unwrap(),
            "cancelled" => update_status(path, SessionState::Closed, None, None).unwrap(),
            "expired" => receipt.deadline_unix_ms = 0,
            "already spawned" => receipt.phase = launch::Phase::Spawned,
            "claim released" => {
                std::fs::remove_file(path.join(crate::native::session::test_support::TURN_CLAIM_FILE)).unwrap()
            }
            "directory open to others" => {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap()
            }
            "no start script" => std::fs::remove_file(path.join("ghostty-start.sh")).unwrap(),
            "start script is a link" => {
                std::fs::remove_file(path.join("ghostty-start.sh")).unwrap();
                std::os::unix::fs::symlink(path.join("launch.json"), path.join("ghostty-start.sh"))
                    .unwrap();
            }
            "relative directory" => sent.0 = PathBuf::from(path.file_name().unwrap()),
            _ => unreachable!(),
        }
        write_json_atomic(&path.join(launch::FILE), &receipt).unwrap();
        assert!(verify_launch(&sent.0, &sent.1).is_err(), "{case}");
    }
}
// The whole start on a private PTY: the creation command as Ghostty runs it, keys
// typed before, with and after the launch, and the launch command as the shell job
// that the owner record describes.
#[test]
fn host_on_a_private_pty_starts_only_its_bound_launch_and_no_key_reaches_it() {
    for case in ["launched", "cancelled", "never addressed"] {
        let directory = launch_fixture();
        let path = directory.path();
        let owner = path.join("owner-probe.json");
        let probe = format!(
            "AB_GHOSTTY_PROBE=owner AB_GHOSTTY_PROBE_DIR={} {} {PROBE}; exit $?",
            crate::native::terminal::shell_quote(path.as_os_str()),
            crate::native::terminal::shell_quote(std::env::current_exe().unwrap().as_os_str())
        );
        let mut f = existing();
        f.pty = Some(Pty {
            keys_before: b"aaaa\n",
            keys_with: b"more\x03\x15",
            keys_after: b"zz\n",
            ..Pty::default()
        });
        let handle = create_with_runner(&mut f, case == "cancelled", soon()).unwrap();
        f.pty.as_mut().unwrap().wait_until_raw();
        assert!(!owner.exists(), "{case}: the launch ran before the binding");
        bind_fixture(path, &handle);
        if case == "cancelled" {
            crate::native::session::update_status(path, SessionState::Closed, None, None).unwrap();
        }
        if case != "never addressed" {
            start(&mut f, &handle, &probe, path, soon()).unwrap();
        }
        let pty = f.pty.as_mut().unwrap();
        let status = pty.run(|| false, Duration::from_secs(10));
        if case != "launched" {
            assert!(!owner.exists(), "{case}: {}", pty.shown());
            assert!(
                matches!(status, Some(code) if code != 0),
                "{case}: {status:?}"
            );
            continue;
        }
        assert_eq!(status, Some(0), "{}", pty.shown());
        let owner: serde_json::Value =
            serde_json::from_slice(&std::fs::read(owner).unwrap()).unwrap();
        assert_eq!(
            owner["queued_input"], 0,
            "a typed key reached the launch: {owner}"
        );
        assert_eq!(owner["group"], owner["terminal_group"], "{owner}");
        assert!(owner["shell"].is_u64(), "{owner}");
        assert!(
            !pty.shown().contains("more"),
            "typed keys were echoed: {}",
            pty.shown()
        );
    }
}
