use super::*;
use crate::native::clock::FakeClock;
use crate::native::session::CoreRecord;
use crate::native::session::primitives::unix_ms;
use crate::native::terminal::macos::{iterm2, wezterm};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    },
    process::{Child, Command, Stdio},
    rc::Rc,
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
    let receipt_written = Instant::now();
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
    // While the capture runs, the preparing record holds the host's budget, which the launch
    // log names: the 10 s receipt less the 2.5 s of reserves, measured when the host started.
    let host::HostObservation::Recorded(preparing) = host::observe(&store, Some(&receipt)) else {
        panic!("missing host record");
    };
    let since_receipt = receipt_written.elapsed().as_millis() as u64;
    assert_eq!(preparing.stage, host::Stage::EnvironmentPreparing);
    let estimate = preparing.capture_budget_ms;
    assert!(
        estimate <= 7_500 && estimate + since_receipt >= 7_500,
        "{estimate} ms, {since_receipt} ms after the receipt"
    );
    let log = fs::read_to_string(directory.path().join(launch::LOG)).unwrap();
    assert!(
        log.contains(&format!(
            " shell_environment_started budget_ms={estimate}\n"
        )),
        "{log}"
    );
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
    // The capture decided its budget when it started, after the host's estimate.
    assert!(
        record.capture_budget_ms <= estimate && record.capture_budget_ms > 0,
        "{} ms enforced, {estimate} ms estimated",
        record.capture_budget_ms
    );
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

// The host's step decision on a scripted clock: the capture and the binding answer from a
// script, a sleep only advances the clock (and a fixed wall clock with it), and the trace
// records every port call at its simulated time in milliseconds from the start. Nothing here
// waits on the wall clock or needs a terminal.
#[derive(Debug, Eq, PartialEq)]
enum Event {
    Poll(u64),
    Ready(u64),
    Observe(u64),
    Sleep(u64),
}
use Event::{Observe, Poll, Ready, Sleep};

#[derive(Clone)]
struct Trace {
    origin: Instant,
    now: Rc<Cell<Instant>>,
    events: Rc<RefCell<Vec<Event>>>,
}

impl Trace {
    fn new(clock: &FakeClock) -> Self {
        let now = clock.shared();
        Self {
            origin: now.get(),
            now,
            events: Rc::default(),
        }
    }

    fn push(&self, event: fn(u64) -> Event) {
        let at = self.now.get().duration_since(self.origin).as_millis() as u64;
        self.events.borrow_mut().push(event(at));
    }
}

struct TracedClock {
    clock: FakeClock,
    trace: Trace,
}

impl Clock for TracedClock {
    fn now(&mut self) -> Instant {
        self.clock.now()
    }

    fn sleep(&mut self, duration: Duration) {
        let ms = duration.as_millis();
        self.trace.events.borrow_mut().push(Sleep(ms as u64));
        self.clock.sleep(duration);
        let wall = &crate::native::session::tests::FIXED_UNIX_MS;
        if let Some(now) = wall.get() {
            wall.set(Some(now + ms));
        }
    }
}

struct ScriptedCapture {
    answers: VecDeque<CapturePoll>,
    trace: Trace,
}

impl CaptureResult for ScriptedCapture {
    fn poll(&mut self) -> CapturePoll {
        self.trace.push(Poll);
        // An exhausted script keeps the capture running.
        self.answers.pop_front().unwrap_or(CapturePoll::Running)
    }
}

struct ScriptedBinding {
    answers: VecDeque<Binding>,
    trace: Trace,
}

impl BindingObservation for ScriptedBinding {
    fn observe(&mut self) -> Binding {
        self.trace.push(Observe);
        // An exhausted script keeps the binding absent.
        self.answers.pop_front().unwrap_or(Binding::Absent)
    }
}

// The production observation, traced.
struct TracedBinding<'a> {
    binding: LockedBinding<'a>,
    trace: Trace,
}

impl BindingObservation for TracedBinding<'_> {
    fn observe(&mut self) -> Binding {
        self.trace.push(Observe);
        self.binding.observe()
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    HandOff { variables: usize },
    Refused(Refusal),
    Error(String),
}

/// The budget a scripted capture enforced: lower than the 10 s the admitted host's preparing
/// record holds, as for a capture that starts after the host's estimate.
const ENFORCED: Duration = Duration::from_millis(9_996);

fn captured() -> Captured {
    Captured {
        variables: vec![("FIXTURE".into(), "ready".into())],
        elapsed: Duration::from_millis(7),
        background_children_stopped: 0,
        budget: ENFORCED,
    }
}

fn capture_failure(
    reason: host::FailureReason,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> CaptureFailure {
    CaptureFailure {
        reason,
        exit_code,
        signal,
        elapsed: Duration::from_millis(1500),
        budget: ENFORCED,
    }
}

fn refusal(reason: host::FailureReason) -> Outcome {
    Outcome::Refused(Refusal::host(reason))
}

/// Drives the wait with scripted ports from its start to its outcome, with the deadline
/// `deadline_ms` after the start. The ready act records its call and answers `ready`.
fn drive(
    deadline_ms: u64,
    captures: Vec<CapturePoll>,
    bindings: Vec<Binding>,
    ready: fn() -> Result<()>,
) -> (Outcome, Vec<Event>) {
    let clock = FakeClock::new(Instant::now());
    let trace = Trace::new(&clock);
    let start = trace.origin;
    let mut clock = TracedClock {
        clock,
        trace: trace.clone(),
    };
    let ready_trace = trace.clone();
    let outcome = await_handoff(
        &mut clock,
        start,
        start + Duration::from_millis(deadline_ms),
        &mut ScriptedCapture {
            answers: captures.into(),
            trace: trace.clone(),
        },
        &mut ScriptedBinding {
            answers: bindings.into(),
            trace: trace.clone(),
        },
        &mut |_, now| {
            assert_eq!(now, ready_trace.now.get());
            ready_trace.push(Ready);
            ready()
        },
    );
    let outcome = match outcome {
        Ok(Ok(captured)) => Outcome::HandOff {
            variables: captured.variables.len(),
        },
        Ok(Err(refusal)) => Outcome::Refused(refusal),
        Err(error) => Outcome::Error(format!("{error:#}")),
    };
    (outcome, trace.events.take())
}

fn recorded() -> Result<()> {
    Ok(())
}

#[test]
fn capture_ready_before_the_binding_hands_off_when_the_binding_names_the_host() {
    let (outcome, trace) = drive(
        1000,
        vec![CapturePoll::Running, CapturePoll::Finished(Ok(captured()))],
        vec![Binding::Absent, Binding::Absent, Binding::Host],
        recorded,
    );
    assert_eq!(outcome, Outcome::HandOff { variables: 1 });
    // The capture is no longer polled once it has answered.
    assert_eq!(
        trace,
        [
            Poll(0),
            Observe(0),
            Sleep(10),
            Poll(10),
            Ready(10),
            Observe(10),
            Sleep(10),
            Observe(20)
        ]
    );
}

#[test]
fn capture_ready_after_the_binding_hands_off_in_the_same_step() {
    let (outcome, trace) = drive(
        1000,
        vec![
            CapturePoll::Running,
            CapturePoll::Running,
            CapturePoll::Finished(Ok(captured())),
        ],
        vec![Binding::Host, Binding::Host, Binding::Host],
        recorded,
    );
    assert_eq!(outcome, Outcome::HandOff { variables: 1 });
    assert_eq!(
        trace,
        [
            Poll(0),
            Observe(0),
            Sleep(10),
            Poll(10),
            Observe(10),
            Sleep(10),
            Poll(20),
            Ready(20),
            Observe(20)
        ]
    );
}

#[test]
fn capture_failure_refuses_with_the_captures_own_facts() {
    for (exit_code, signal) in [(Some(3), None), (None, Some(9))] {
        let (outcome, trace) = drive(
            1000,
            vec![
                CapturePoll::Running,
                CapturePoll::Finished(Err(capture_failure(
                    host::FailureReason::CaptureExitNonzero,
                    exit_code,
                    signal,
                ))),
            ],
            vec![],
            recorded,
        );
        assert_eq!(
            outcome,
            Outcome::Refused(Refusal {
                reason: host::FailureReason::CaptureExitNonzero,
                exit_code,
                signal,
                capture_elapsed: Some(Duration::from_millis(1500)),
                capture_budget: Some(ENFORCED),
            })
        );
        // The binding is not observed after the capture failed.
        assert_eq!(trace, [Poll(0), Observe(0), Sleep(10), Poll(10)]);
    }
}

#[test]
fn a_capture_thread_gone_without_an_answer_refuses_as_a_failed_capture_spawn() {
    let (outcome, trace) = drive(
        1000,
        vec![
            CapturePoll::Running,
            CapturePoll::Running,
            CapturePoll::Gone,
        ],
        vec![],
        recorded,
    );
    // The capture's duration is the host's own time since the start.
    assert_eq!(
        outcome,
        Outcome::Refused(Refusal {
            reason: host::FailureReason::CaptureSpawnFailed,
            exit_code: None,
            signal: None,
            capture_elapsed: Some(Duration::from_millis(20)),
            // The thread gave no answer, so no enforced budget.
            capture_budget: None,
        })
    );
    assert_eq!(
        trace,
        [
            Poll(0),
            Observe(0),
            Sleep(10),
            Poll(10),
            Observe(10),
            Sleep(10),
            Poll(20)
        ]
    );
}

#[test]
fn a_binding_that_does_not_name_the_host_refuses_at_once_with_or_without_the_capture() {
    for binding in [Binding::Other, Binding::Unavailable] {
        // The capture still runs: the host refuses without waiting for it.
        let (outcome, trace) = drive(1000, vec![], vec![Binding::Absent, binding], recorded);
        assert_eq!(outcome, refusal(host::FailureReason::BindingMismatch));
        assert_eq!(
            trace,
            [Poll(0), Observe(0), Sleep(10), Poll(10), Observe(10)],
            "{binding:?}"
        );
        let (outcome, trace) = drive(
            1000,
            vec![CapturePoll::Finished(Ok(captured()))],
            vec![binding],
            recorded,
        );
        assert_eq!(outcome, refusal(host::FailureReason::BindingMismatch));
        assert_eq!(trace, [Poll(0), Ready(0), Observe(0)], "{binding:?}");
    }
}

#[test]
fn the_deadline_refuses_before_the_binding_is_observed_with_and_without_the_capture() {
    let (outcome, trace) = drive(30, vec![], vec![], recorded);
    assert_eq!(outcome, refusal(host::FailureReason::BindingTimeout));
    assert_eq!(
        trace,
        [
            Poll(0),
            Observe(0),
            Sleep(10),
            Poll(10),
            Observe(10),
            Sleep(10),
            Poll(20),
            Observe(20),
            Sleep(10),
            Poll(30)
        ]
    );
    let (outcome, trace) = drive(
        30,
        vec![CapturePoll::Finished(Ok(captured()))],
        vec![],
        recorded,
    );
    assert_eq!(outcome, refusal(host::FailureReason::BindingTimeout));
    assert_eq!(
        trace,
        [
            Poll(0),
            Ready(0),
            Observe(0),
            Sleep(10),
            Observe(10),
            Sleep(10),
            Observe(20),
            Sleep(10)
        ]
    );
}

#[test]
fn a_capture_answer_at_the_deadline_is_taken_before_the_deadline() {
    let failure = capture_failure(host::FailureReason::CaptureTimeout, None, None);
    let (outcome, trace) = drive(
        0,
        vec![CapturePoll::Finished(Err(failure))],
        vec![],
        recorded,
    );
    assert_eq!(
        outcome,
        Outcome::Refused(Refusal {
            reason: host::FailureReason::CaptureTimeout,
            exit_code: None,
            signal: None,
            capture_elapsed: Some(Duration::from_millis(1500)),
            capture_budget: Some(ENFORCED),
        })
    );
    assert_eq!(trace, [Poll(0)]);
    // A ready environment is recorded first; the deadline then refuses unobserved.
    let (outcome, trace) = drive(
        0,
        vec![CapturePoll::Finished(Ok(captured()))],
        vec![Binding::Host],
        recorded,
    );
    assert_eq!(outcome, refusal(host::FailureReason::BindingTimeout));
    assert_eq!(trace, [Poll(0), Ready(0)]);
}

#[test]
fn a_failed_ready_record_ends_the_wait_without_a_refusal() {
    let (outcome, trace) = drive(
        1000,
        vec![CapturePoll::Finished(Ok(captured()))],
        vec![Binding::Host],
        || Err(anyhow::anyhow!("stale launch host transition")),
    );
    assert_eq!(
        outcome,
        Outcome::Error("stale launch host transition".into())
    );
    assert_eq!(trace, [Poll(0), Ready(0)]);
}

// The capture result port's production adapter: the capture thread's channel.
#[test]
fn the_capture_channel_answers_running_then_each_result_then_gone() {
    let (send, receive) = mpsc::sync_channel(1);
    let mut capture = ChannelCapture(receive);
    assert!(matches!(capture.poll(), CapturePoll::Running));
    send.send(Ok(captured())).unwrap();
    assert!(matches!(
        capture.poll(),
        CapturePoll::Finished(Ok(captured)) if captured.variables.len() == 1
    ));
    send.send(Err(capture_failure(
        host::FailureReason::CaptureTimeout,
        None,
        None,
    )))
    .unwrap();
    assert!(matches!(
        capture.poll(),
        CapturePoll::Finished(Err(failure)) if failure.reason == host::FailureReason::CaptureTimeout
    ));
    drop(send);
    assert!(matches!(capture.poll(), CapturePoll::Gone));
}

const IDENTITY: host::HostIdentity = host::HostIdentity {
    pid: 123,
    start_seconds: 1234,
    start_microseconds: 5,
};

// An iTerm2 host admitted for a pending launch, preparing its environment.
fn admitted(timeout: Duration) -> (tempfile::TempDir, Store, String) {
    let (directory, store, token) =
        launch::test_support::pending_launch("session-", &std::env::temp_dir(), timeout);
    host::initialize(&store, &token, TerminalKind::Iterm2).unwrap();
    host::admit(&store, &token, IDENTITY).unwrap();
    host::advance(
        &store,
        &token,
        IDENTITY,
        host::Stage::EnvironmentPreparing,
        host::Progress {
            capture_budget_ms: 10_000,
            ..Default::default()
        },
    )
    .unwrap();
    (directory, store, token)
}

fn session_id(store: &Store) -> &str {
    store.directory().file_name().unwrap().to_str().unwrap()
}

fn host_record(store: &Store) -> host::HostRecord {
    match host::observe(store, launch::read(store).unwrap().as_ref()) {
        host::HostObservation::Recorded(record) => record,
        other => panic!("{other:?}"),
    }
}

/// Runs the admitted host's wait on the scripted clock and capture, with the production
/// binding observation and record acts, until it refuses; the deadline comes from the
/// receipt as the host computes it. Returns the host's error and the trace.
fn refuse(store: &Store, token: &str, captures: Vec<CapturePoll>) -> (String, Vec<Event>) {
    let clock = FakeClock::new(Instant::now());
    let trace = Trace::new(&clock);
    let start = trace.origin;
    let mut clock = TracedClock {
        clock,
        trace: trace.clone(),
    };
    let receipt = launch::read(store).unwrap().unwrap();
    let deadline = receipt.monotonic_deadline(start, Duration::ZERO);
    let recorder = Recorder {
        store,
        token,
        kind: TerminalKind::Iterm2,
        identity: Some(IDENTITY),
        start,
    };
    let ready_trace = trace.clone();
    let refusal = await_handoff(
        &mut clock,
        start,
        deadline,
        &mut ScriptedCapture {
            answers: captures.into(),
            trace: trace.clone(),
        },
        &mut TracedBinding {
            binding: LockedBinding {
                store,
                token,
                identity: IDENTITY,
                session_id: session_id(store),
                kind: TerminalKind::Iterm2,
            },
            trace: trace.clone(),
        },
        &mut |captured, now| {
            ready_trace.push(Ready);
            recorder.ready(IDENTITY, captured, now)
        },
    )
    .unwrap()
    .expect_err("the host refuses");
    let error = recorder.refuse(refusal, clock.now());
    (format!("{error:#}"), trace.events.take())
}

fn launch_log(store: &Store) -> String {
    fs::read_to_string(store.directory().join(launch::LOG)).unwrap()
}

#[test]
fn capture_failure_is_recorded_while_binding_is_absent() {
    let (_directory, store, token) = admitted(Duration::from_secs(10));
    let (error, trace) = refuse(
        &store,
        &token,
        vec![
            CapturePoll::Running,
            CapturePoll::Finished(Err(capture_failure(
                host::FailureReason::CaptureExitNonzero,
                Some(3),
                None,
            ))),
        ],
    );
    assert_eq!(error, "iTerm2 launch host refused: capture_exit_nonzero");
    assert_eq!(trace, [Poll(0), Observe(0), Sleep(10), Poll(10)]);
    let record = host_record(&store);
    assert_eq!(
        record.failure,
        Some(host::Failure {
            reason: host::FailureReason::CaptureExitNonzero,
            exit_code: Some(3),
            signal: None,
        })
    );
    assert_eq!(record.stage, host::Stage::EnvironmentPreparing);
    assert_eq!(record.elapsed_ms, 10);
    assert_eq!(record.capture_elapsed_ms, Some(1500));
    let log = launch_log(&store);
    assert!(
        log.contains(" shell_environment_failed reason=capture_exit_nonzero elapsed_ms=1500\n"),
        "{log}"
    );
    assert!(
        log.ends_with(" iterm2_host_refused reason=capture_exit_nonzero\n"),
        "{log}"
    );
}

// A capture that timed out: its failure record's budget is the one `status.error` names.
#[test]
fn a_capture_timeout_records_the_budget_that_status_error_names() {
    let (_directory, store, token) = admitted(Duration::from_secs(10));
    let (error, trace) = refuse(
        &store,
        &token,
        vec![CapturePoll::Finished(Err(capture_failure(
            host::FailureReason::CaptureTimeout,
            None,
            None,
        )))],
    );
    assert_eq!(error, "iTerm2 launch host refused: capture_timeout");
    assert_eq!(trace, [Poll(0)]);
    let record = host_record(&store);
    assert_eq!(record.stage, host::Stage::EnvironmentPreparing);
    // The preparing record's 10 s estimate is replaced by the budget the capture enforced.
    assert_eq!(record.capture_budget_ms, 9_996);
    assert_eq!(record.capture_elapsed_ms, Some(1500));
    assert!(launch::repair(&store).unwrap());
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(
            "terminal shell environment could not be prepared: startup files did not finish within 9996 ms (capture_timeout); last recorded launch stage: environment_preparing"
        )
    );
}

#[test]
fn binding_failure_is_recorded_while_capture_is_still_running() {
    let (_directory, store, token) = admitted(Duration::from_secs(10));
    bind(&store, TerminalKind::WezTerm);
    // The capture never answers: the refusal comes at the first observation.
    let (error, trace) = refuse(&store, &token, vec![]);
    assert_eq!(error, "iTerm2 launch host refused: binding_mismatch");
    assert_eq!(trace, [Poll(0), Observe(0)]);
    let record = host_record(&store);
    assert_eq!(
        record.failure,
        Some(host::Failure {
            reason: host::FailureReason::BindingMismatch,
            exit_code: None,
            signal: None,
        })
    );
    assert_eq!(record.stage, host::Stage::EnvironmentPreparing);
    assert_eq!(record.capture_elapsed_ms, None);
    // The capture has not answered: the preparing record's budget stays.
    assert_eq!(record.capture_budget_ms, 10_000);
    let log = launch_log(&store);
    assert!(!log.contains("shell_environment_failed"), "{log}");
    assert!(
        log.ends_with(" iterm2_host_refused reason=binding_mismatch\n"),
        "{log}"
    );
}

#[test]
fn binding_timeout_is_retained_after_receipt_expiry() {
    struct Wall(Option<u128>);
    impl Drop for Wall {
        fn drop(&mut self) {
            crate::native::session::tests::FIXED_UNIX_MS.set(self.0);
        }
    }
    const WALL: u128 = 1_000_000;
    let _wall = Wall(crate::native::session::tests::FIXED_UNIX_MS.replace(Some(WALL)));
    let (_directory, store, token) = admitted(Duration::from_secs(10));
    // A receipt that expires 50 ms after the host starts, on the fixed wall clock.
    let mut receipt = launch::read(&store).unwrap().unwrap();
    receipt.deadline_unix_ms = WALL + 50;
    store
        .record(CoreRecord::Launch)
        .write_json(&receipt)
        .unwrap();
    let (error, trace) = refuse(&store, &token, vec![CapturePoll::Finished(Ok(captured()))]);
    // The binding was last observed while the receipt was current; the failure is
    // recorded once the receipt has expired.
    assert_eq!(unix_ms(), WALL + 50);
    assert_eq!(error, "iTerm2 launch host refused: binding_timeout");
    assert_eq!(
        trace,
        [
            Poll(0),
            Ready(0),
            Observe(0),
            Sleep(10),
            Observe(10),
            Sleep(10),
            Observe(20),
            Sleep(10),
            Observe(30),
            Sleep(10),
            Observe(40),
            Sleep(10)
        ]
    );
    let record = host_record(&store);
    assert_eq!(
        record.failure.map(|failure| failure.reason),
        Some(host::FailureReason::BindingTimeout)
    );
    assert_eq!(record.stage, host::Stage::EnvironmentReady);
    assert_eq!(record.elapsed_ms, 50);
    // The ready record replaced the preparing estimate with the budget the capture enforced,
    // and the binding failure keeps it.
    assert_eq!(record.capture_budget_ms, 9_996);
    assert_eq!(record.capture_elapsed_ms, Some(7));
    assert_eq!(record.variables, Some(1));
    assert_eq!(record.background_children_stopped, Some(0));
    let log = launch_log(&store);
    assert!(
        log.contains(
            " shell_environment_ready elapsed_ms=7 variables=1 background_children_stopped=0\n"
        ),
        "{log}"
    );
    assert!(!log.contains("shell_environment_failed"), "{log}");
    assert!(
        log.ends_with(" iterm2_host_refused reason=binding_timeout\n"),
        "{log}"
    );
}

// The surface observation port's production adapter, on a real Session directory. A binding
// that names this host's surface depends on the terminal's environment; the PTY regression
// tests and the surface identity tests cover it.
#[test]
fn the_locked_binding_observation_needs_an_eligible_host_and_a_readable_handle() {
    use launch::test_support::{Change, apply};
    let other = host::HostIdentity {
        pid: 124,
        ..IDENTITY
    };
    let mut failures = Vec::new();
    for (case, observer, expected) in [
        ("absent", IDENTITY, Binding::Absent),
        ("another terminal", IDENTITY, Binding::Other),
        ("damaged handle", IDENTITY, Binding::Unavailable),
        ("expired receipt", IDENTITY, Binding::Unavailable),
        ("failed host", IDENTITY, Binding::Unavailable),
        ("another incarnation", other, Binding::Unavailable),
    ] {
        let (_directory, store, token) = admitted(Duration::from_secs(10));
        match case {
            "another terminal" => bind(&store, TerminalKind::WezTerm),
            "damaged handle" => fs::write(store.record(CoreRecord::Terminal).path(), b"{").unwrap(),
            "expired receipt" => apply(&store, &[Change::ReceiptExpired]),
            "failed host" => host::fail(
                &store,
                &token,
                Some(IDENTITY),
                host::Failure {
                    reason: host::FailureReason::CaptureTimeout,
                    exit_code: None,
                    signal: None,
                },
                1,
                None,
                None,
            )
            .unwrap(),
            _ => {}
        }
        let observed = LockedBinding {
            store: &store,
            token: &token,
            identity: observer,
            session_id: session_id(&store),
            kind: TerminalKind::Iterm2,
        }
        .observe();
        if observed != expected {
            failures.push(format!("{case}: {observed:?} != {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
