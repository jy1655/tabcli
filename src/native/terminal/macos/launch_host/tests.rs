use super::*;
use crate::native::session::CoreRecord;
use crate::native::terminal::macos::{iterm2, wezterm};
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
    let binary = crate::native::tests::package_binary();
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
    let directory =
        launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(10)).0;
    let store = Store::open_unchecked(directory.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, kind).unwrap();
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
    let dir =
        launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(10)).0;
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, TerminalKind::Iterm2).unwrap();
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
    let dir =
        launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(10)).0;
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, TerminalKind::Iterm2).unwrap();
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
    let dir =
        launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(3)).0;
    let store = Store::open_unchecked(dir.path());
    let receipt = launch::read(&store).unwrap().unwrap();
    host::initialize(&store, &receipt.claim_token, TerminalKind::Iterm2).unwrap();
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

#[test]
fn eligibility_reports_the_first_failing_fact_then_the_host_record() {
    use crate::native::session::launch::test_support::{Change::*, Expect, apply};
    const NOT: Expect = Expect::Text("launch host is no longer eligible");
    let identity = host::HostIdentity {
        pid: 123,
        start_seconds: 1234,
        start_microseconds: 5,
    };
    let other = host::HostIdentity {
        pid: 124,
        ..identity
    };
    let cases = [
        ("admitted", vec![], identity, Expect::Pass),
        (
            "receipt missing",
            vec![ReceiptMissing],
            identity,
            Expect::Text("missing launch receipt"),
        ),
        (
            "receipt damaged",
            vec![ReceiptDamaged],
            identity,
            Expect::Receipt,
        ),
        (
            "receipt before status",
            vec![ReceiptDamaged, StatusDamaged],
            identity,
            Expect::Receipt,
        ),
        ("receipt token", vec![ReceiptToken], identity, NOT),
        ("spawning", vec![ReceiptSpawning], identity, NOT),
        ("expired", vec![ReceiptExpired], identity, NOT),
        ("status closed", vec![StatusClosed], identity, NOT),
        (
            "status damaged",
            vec![StatusDamaged],
            identity,
            Expect::Status,
        ),
        ("tombstone", vec![Tombstone], identity, NOT),
        (
            "tombstone damaged",
            vec![TombstoneDamaged],
            identity,
            Expect::Tombstone,
        ),
        ("claim absent", vec![ClaimAbsent], identity, NOT),
        ("claim other", vec![ClaimOther], identity, NOT),
        (
            "claim unreadable",
            vec![ClaimUnreadable],
            identity,
            Expect::Claim,
        ),
        ("another incarnation", vec![], other, NOT),
        (
            "damaged status with spawning",
            vec![ReceiptSpawning, StatusDamaged],
            identity,
            Expect::Status,
        ),
        (
            "damaged status with expiry",
            vec![ReceiptExpired, StatusDamaged],
            identity,
            Expect::Status,
        ),
        (
            "damaged tombstone with spawning",
            vec![ReceiptSpawning, TombstoneDamaged],
            identity,
            Expect::Tombstone,
        ),
        (
            "unreadable claim before the host record",
            vec![ClaimUnreadable],
            other,
            Expect::Claim,
        ),
    ];
    let mut failures = Vec::new();
    for (case, changes, caller, expect) in cases {
        let root = tempfile::tempdir().unwrap();
        let directory =
            launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(10))
                .0;
        let store = Store::open_unchecked(directory.path());
        let token = launch::read(&store).unwrap().unwrap().claim_token;
        host::initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        host::admit(&store, &token, identity).unwrap();
        apply(&store, &changes);
        let expected = expect.error(&store);
        let actual = host::eligible_incarnation(&store, &token, caller)
            .err()
            .map(|error| format!("{error:#}"));
        if actual != expected {
            failures.push(format!("{case}: {actual:?} != {expected:?}"));
        }
    }
    // A host record that does not name this admitted, unfailed incarnation.
    for case in ["unadmitted", "failed", "damaged"] {
        let root = tempfile::tempdir().unwrap();
        let directory =
            launch::test_support::pending_launch("session-", root.path(), Duration::from_secs(10))
                .0;
        let store = Store::open_unchecked(directory.path());
        let token = launch::read(&store).unwrap().unwrap().claim_token;
        host::initialize(&store, &token, TerminalKind::Iterm2).unwrap();
        match case {
            "unadmitted" => {}
            "failed" => {
                host::admit(&store, &token, identity).unwrap();
                host::fail(
                    &store,
                    &token,
                    Some(identity),
                    host::Failure {
                        reason: host::FailureReason::CaptureTimeout,
                        exit_code: None,
                        signal: None,
                    },
                    1,
                    None,
                )
                .unwrap();
            }
            "damaged" => fs::write(store.record(CoreRecord::LaunchHost).path(), b"{").unwrap(),
            _ => unreachable!(),
        }
        let actual = host::eligible_incarnation(&store, &token, identity)
            .err()
            .map(|error| format!("{error:#}"));
        if actual.as_deref() != Some("launch host is no longer eligible") {
            failures.push(format!("{case}: {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Runs only as a child of the test below, which gives it the terminal environment.
#[test]
fn surface_identity_probe() {
    let Ok(mode) = std::env::var("AB_LAUNCH_HOST_IDENTITY_PROBE") else {
        return;
    };
    let surface = |kind, id: &str, socket: Option<&str>| TerminalSession {
        kind,
        id: id.into(),
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        wezterm_mux: socket.map(|socket| crate::native::terminal::WezTermMux {
            socket: socket.into(),
            pid: 1,
            start_seconds: 1,
            start_microseconds: 1,
            owns_gui: true,
        }),
        windows_process_identity: None,
    };
    let surfaces = [
        (
            "iterm2 owned",
            surface(TerminalKind::Iterm2, "owned-id", None),
        ),
        (
            "iterm2 other",
            surface(TerminalKind::Iterm2, "other-id", None),
        ),
        (
            "wezterm owned",
            surface(TerminalKind::WezTerm, "owned-id", Some("/fixture/socket")),
        ),
        (
            "wezterm owned without mux",
            surface(TerminalKind::WezTerm, "owned-id", None),
        ),
        (
            "wezterm owned other gui",
            surface(TerminalKind::WezTerm, "owned-id", Some("/fixture/other")),
        ),
        (
            "wezterm other pane",
            surface(TerminalKind::WezTerm, "other-id", Some("/fixture/socket")),
        ),
        (
            "ghostty owned",
            surface(TerminalKind::Ghostty, "owned-id", Some("/fixture/socket")),
        ),
    ];
    let mut matched = Vec::new();
    for (name, surface) in &surfaces {
        for kind in TerminalKind::ALL {
            if matches_surface(surface, kind) {
                matched.push(format!("{name} as {kind:?}"));
            }
        }
    }
    println!("MATCHED {mode}: {}", matched.join(", "));
}

// Whether a bound surface names this host is read from the environment the terminal gives
// the host: iTerm2's session id after its last colon, WezTerm's pane and GUI socket.
#[test]
fn launch_host_names_its_surface_from_its_terminal_environment() {
    const OWNED_ITERM: (&str, &str) = ("ITERM_SESSION_ID", "w0t0p0:owned-id");
    const OWNED_PANE: (&str, &str) = ("WEZTERM_PANE", "owned-id");
    const OWNED_SOCKET: (&str, &str) = ("WEZTERM_UNIX_SOCKET", "/fixture/socket");
    let cases = [
        (
            "all",
            vec![OWNED_ITERM, OWNED_PANE, OWNED_SOCKET],
            "iterm2 owned as Iterm2, wezterm owned as WezTerm",
        ),
        ("none", vec![], ""),
        (
            "iterm bare",
            vec![("ITERM_SESSION_ID", "owned-id")],
            "iterm2 owned as Iterm2",
        ),
        ("iterm empty", vec![("ITERM_SESSION_ID", "w0t0p0:")], ""),
        (
            "iterm other",
            vec![("ITERM_SESSION_ID", "w0t0p0:other-id")],
            "iterm2 other as Iterm2",
        ),
        ("wezterm pane only", vec![OWNED_PANE], ""),
        ("wezterm socket only", vec![OWNED_SOCKET], ""),
        (
            "wezterm socket spelling",
            vec![OWNED_PANE, ("WEZTERM_UNIX_SOCKET", "/fixture//socket")],
            "wezterm owned as WezTerm",
        ),
        (
            "wezterm other pane",
            vec![("WEZTERM_PANE", "other-id"), OWNED_SOCKET],
            "wezterm other pane as WezTerm",
        ),
        (
            "wezterm other gui",
            vec![OWNED_PANE, ("WEZTERM_UNIX_SOCKET", "/fixture/other")],
            "wezterm owned other gui as WezTerm",
        ),
        (
            "wezterm empty pane",
            vec![("WEZTERM_PANE", ""), OWNED_SOCKET],
            "",
        ),
        (
            "iterm environment only names iterm",
            vec![OWNED_ITERM],
            "iterm2 owned as Iterm2",
        ),
    ];
    for (mode, environment, expected) in cases {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native::terminal::macos::launch_host::tests::surface_identity_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_remove("ITERM_SESSION_ID")
            .env_remove("WEZTERM_PANE")
            .env_remove("WEZTERM_UNIX_SOCKET")
            .envs(environment)
            .env("AB_LAUNCH_HOST_IDENTITY_PROBE", mode)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{mode}: {stdout}");
        let prefix = format!("MATCHED {mode}: ");
        let line = stdout
            .lines()
            .find_map(|line| line.split_once(&prefix).map(|(_, matched)| matched))
            .unwrap_or_else(|| panic!("{mode}: {stdout}"));
        assert_eq!(line, expected, "{mode}");
    }
}
