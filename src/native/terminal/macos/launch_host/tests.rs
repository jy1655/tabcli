use super::*;
use crate::native::terminal::macos::{iterm2, test_support, wezterm};
use std::{
    fs,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    },
    process::{Child, Command, Stdio},
};

struct PtyChild {
    child: Child,
    master: Option<fs::File>,
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        // macOS can wait for the PTY output queue while its session leader exits.
        // Close the fixture's master before reaping, never retain it across wait.
        drop(self.master.take());
        // Only this fixture's process group; retain its unreaped PID until signaling.
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn spawn(program: &[String], home: &Path) -> PtyChild {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let mut command = Command::new(&program[0]);
    command
        .args(&program[1..])
        .env_clear()
        .env("HOME", home)
        .env("ZDOTDIR", home)
        .env("PATH", "/usr/bin:/bin")
        .env("ITERM_SESSION_ID", "w0t0p0:owned-id")
        .env("WEZTERM_PANE", "owned-id")
        .env("WEZTERM_UNIX_SOCKET", "/fixture/socket")
        .stdin(slave.try_clone().unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    PtyChild {
        child: command.spawn().unwrap(),
        master: Some(master),
    }
}
fn wait_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}
fn bind(store: &Store, kind: TerminalKind) {
    store
        .write_terminal(&TerminalSession {
            kind,
            id: "owned-id".into(),
            tab_id: None,
            window_id: None,
            managed_session_id: Some(
                store
                    .directory()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
            ),
            wezterm_mux: (kind == TerminalKind::WezTerm).then(|| {
                crate::native::terminal::WezTermMux {
                    socket: "/fixture/socket".into(),
                    pid: 1,
                    start_seconds: 1,
                    start_microseconds: 1,
                    owns_gui: true,
                }
            }),
            windows_process_identity: None,
        })
        .unwrap();
}
fn bootstrap(executable: &Path, dir: &Path, command: &str, kind: TerminalKind) -> Vec<String> {
    if let Some(baseline) = std::env::var_os("BRIDGE_376_BASELINE_BUILDER") {
        // The caller compiles these two pure builders extracted verbatim from bb33f64.
        let host = format!(
            "{} {} {}",
            crate::native::terminal::shell_quote(executable.as_os_str()),
            kind.registration().host_command,
            crate::native::terminal::shell_quote(dir.as_os_str())
        );
        let output = Command::new(baseline)
            .args([
                if kind == TerminalKind::Iterm2 {
                    "iterm2"
                } else {
                    "wezterm"
                },
                &host,
                command,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let raw = String::from_utf8(output.stdout).unwrap();
        if kind == TerminalKind::Iterm2 {
            return iterm2::test_iterm_arguments(&raw);
        }
        return raw.split_terminator('\0').map(str::to_owned).collect();
    }
    match kind {
        TerminalKind::Iterm2 => {
            iterm2::test_iterm_arguments(&iterm2::host_command(executable, dir, command))
        }
        TerminalKind::WezTerm => wezterm::host_program(executable, dir, command).into(),
        _ => unreachable!(),
    }
}
fn executable(root: &Path) -> std::path::PathBuf {
    let path = root.join("host '\" $$ \\ fixture");
    // --all-targets builds the real entry point beside the unit-test executable.
    // Exercise production argument dispatch and exec rather than a libtest-specific route.
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tabcli");
    assert!(
        binary.is_file(),
        "run this regression with cargo test --all-targets"
    );
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf yes > \"$2/host-reached\"\nexec {} \"$@\"\n",
            crate::native::terminal::shell_quote(binary.as_os_str())
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn pty_regression(kind: TerminalKind) {
    let root = tempfile::tempdir().unwrap();
    let directory = test_support::launch_fixture("session-", Duration::from_secs(10), root.path());
    let store = Store::open_unchecked(directory.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(
        &store,
        &receipt.claim_token,
        if kind == TerminalKind::Iterm2 {
            host::Terminal::Iterm2
        } else {
            host::Terminal::Wezterm
        },
    )
    .unwrap();
    // This exact fixture blocks the old controlling-terminal bootstrap before the host.
    // Non-TTY capture gets EOF at the key read and then waits for the controlled release.
    fs::write(directory.path().join(".zshrc"), b"read -r -k 1 ignored\nprintf started > $HOME/capturing\nwhile [[ ! -e $HOME/release ]]; do sleep 0.01; done\nexport FIXTURE_VALUE='captured value'\nexport CODEX_THREAD_ID=removed_at_handoff\n").unwrap();
    fs::write(directory.path().join("launch.sh"), b"if read -t 0.1 line; then printf dirty > $HOME/input; else printf clean > $HOME/input; fi\n[[ $FIXTURE_VALUE == 'captured value' && -z ${CODEX_THREAD_ID-} ]] || exit 8\nprintf done > $HOME/handoff\nexit 0\n").unwrap();
    let command = format!(
        ". {}",
        crate::native::terminal::shell_quote(directory.path().join("launch.sh").as_os_str())
    );
    let program = bootstrap(&executable(root.path()), directory.path(), &command, kind);
    let mut child = spawn(&program, directory.path());
    let reached = wait_file(
        &directory.path().join("host-reached"),
        Duration::from_secs(2),
    );
    // For the baseline run this assertion fails, with no input sent to unblock zsh.
    assert!(
        reached,
        "production bootstrap did not reach the host without terminal input ({kind:?})"
    );
    assert!(wait_file(
        &directory.path().join("capturing"),
        Duration::from_secs(2)
    ));
    child
        .master
        .as_mut()
        .unwrap()
        .write_all(b"typed while capture runs\n")
        .unwrap();
    bind(&store, kind);
    fs::write(directory.path().join("release"), b"yes").unwrap();
    assert!(wait_file(
        &directory.path().join("handoff"),
        Duration::from_secs(3)
    ));
    assert_eq!(fs::read(directory.path().join("input")).unwrap(), b"clean");
    let host::HostObservation::Recorded(record) = host::observe(&store, Some(&receipt)) else {
        panic!("missing host record");
    };
    assert_eq!(record.stage, host::Stage::LaunchShellStarting);
    assert!(record.failure.is_none());
    assert!(record.variables.is_some_and(|n| n > 0));
    let log = fs::read_to_string(directory.path().join(launch::LOG)).unwrap();
    for marker in [
        "_host_started",
        "shell_environment_started budget_ms=",
        "shell_environment_ready elapsed_ms=",
        "_host_launch_shell",
    ] {
        assert!(log.contains(marker), "{marker}: {log}");
    }
}
#[test]
fn iterm2_production_bootstrap_bypasses_startup_key_read_and_flushes_capture_input() {
    pty_regression(TerminalKind::Iterm2);
}
#[test]
fn wezterm_production_bootstrap_bypasses_startup_key_read_and_flushes_capture_input() {
    pty_regression(TerminalKind::WezTerm);
}

#[test]
fn capture_failure_is_recorded_while_binding_is_absent() {
    let root = tempfile::tempdir().unwrap();
    let dir = test_support::launch_fixture("session-", Duration::from_secs(10), root.path());
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, host::Terminal::Iterm2).unwrap();
    fs::write(dir.path().join(".zshrc"), "exit 3\n").unwrap();
    let program = bootstrap(
        &executable(root.path()),
        dir.path(),
        "exit 99",
        TerminalKind::Iterm2,
    );
    let mut child = spawn(&program, dir.path());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut recorded = false;
    while Instant::now() < deadline {
        if let host::HostObservation::Recorded(r) = host::observe(&store, Some(&receipt))
            && let Some(f) = r.failure
        {
            assert_eq!(f.reason, host::FailureReason::CaptureExitNonzero);
            assert_eq!(f.exit_code, Some(3));
            assert_eq!(r.stage, host::Stage::EnvironmentPreparing);
            recorded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(recorded);
    let _ = child.child.wait();
}
#[test]
fn binding_failure_is_recorded_while_capture_is_still_running() {
    let root = tempfile::tempdir().unwrap();
    let dir = test_support::launch_fixture("session-", Duration::from_secs(10), root.path());
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, host::Terminal::Iterm2).unwrap();
    fs::write(dir.path().join(".zshrc"), "sleep 100\n").unwrap();
    bind(&store, TerminalKind::WezTerm);
    let program = bootstrap(
        &executable(root.path()),
        dir.path(),
        "exit 99",
        TerminalKind::Iterm2,
    );
    let mut child = spawn(&program, dir.path());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut recorded = false;
    while Instant::now() < deadline {
        if let host::HostObservation::Recorded(r) = host::observe(&store, Some(&receipt))
            && let Some(f) = r.failure
        {
            assert_eq!(f.reason, host::FailureReason::BindingMismatch);
            recorded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(recorded);
    let _ = child.child.wait();
}

#[test]
fn binding_timeout_is_retained_after_receipt_expiry() {
    let root = tempfile::tempdir().unwrap();
    let dir = test_support::launch_fixture("session-", Duration::from_secs(3), root.path());
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, host::Terminal::Iterm2).unwrap();
    fs::write(dir.path().join(".zshrc"), "export FIXTURE=ready\n").unwrap();
    let program = bootstrap(
        &executable(root.path()),
        dir.path(),
        "exit 99",
        TerminalKind::Iterm2,
    );
    let _child = spawn(&program, dir.path());
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        if let host::HostObservation::Recorded(record) = host::observe(&store, Some(&receipt))
            && let Some(failure) = record.failure
        {
            assert_eq!(failure.reason, host::FailureReason::BindingTimeout);
            assert_eq!(record.stage, host::Stage::EnvironmentReady);
            assert!(record.capture_elapsed_ms.is_some());
            assert!(record.variables.is_some());
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("binding timeout was not retained for the expired, still current receipt");
}
