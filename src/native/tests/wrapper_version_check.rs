//! The launch wrapper's re-check of the provider version. The bound `provision` hands to the
//! check is observed through `provision_with`, with the wall clock pinned and no process. The
//! other tests run `provision` against a fake Claude that records its probe's start and that the
//! provider itself was started; each call runs under a hang guard that only turns a call that
//! never returns into a failure. No test here asserts how long a correct call takes.
use crate::native::failure_kind::{FailureKind, TimedOut, classify};
use crate::native::session::launch::{
    self, Phase, Verdict,
    wrapper::{provision, provision_with},
};
use crate::native::session::primitives::{unix_ms, write_json_atomic, write_private};
use crate::native::session::schema::{SESSION_SCHEMA, SessionManifest};
use crate::native::session::{CoreRecord, Reader, SessionState, Store, turn, update_status};
use crate::native::terminal::shell_quote;
use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, thread};

const VERSION: &str = "2.1.302 (Claude Code)";
// A hang guard, not a timing assertion: it only turns a call that never returns into a failure.
// The longest correct call here waits out a recorded remainder and the grace, about 2.4 s.
const GUARD: Duration = Duration::from_secs(30);
// What remains of the recorded launch budget when the hung-probe test's check converts it; the
// unrecorded-deadline test answers later than this.
const RECORDED_BUDGET: Duration = Duration::from_millis(400);
// The owner's grace after the launch deadline (2026-11-11); the launch module's own test pins
// its constant to the same value.
const GRACE: Duration = Duration::from_secs(2);

/// Pins `unix_ms` on the current thread (the override is thread-local) until dropped.
struct PinnedClock(Option<u128>);

impl PinnedClock {
    fn at(now: u128) -> Self {
        Self(crate::native::session::tests::FIXED_UNIX_MS.replace(Some(now)))
    }
}

impl Drop for PinnedClock {
    fn drop(&mut self) {
        crate::native::session::tests::FIXED_UNIX_MS.set(self.0);
    }
}

struct Session {
    _root: tempfile::TempDir,
    root: PathBuf,
    directory: PathBuf,
    provider: PathBuf,
    probe_started: PathBuf,
    provider_started: PathBuf,
}

/// A launching Claude session with a retained claim and no launch record, in a temporary
/// directory whose path no other process carries. `answer` is the fake provider's `--version`
/// body, which runs after the probe recorded its start.
fn session(answer: &str) -> Session {
    let root = tempfile::tempdir().unwrap();
    let provider = root.path().join("fake-claude");
    let probe_started = root.path().join("probe-started");
    let provider_started = root.path().join("provider-started");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  : > {probe}\n  {answer}\nfi\n: > {started}\nexit 0\n",
            probe = shell_quote(probe_started.as_os_str()),
            started = shell_quote(provider_started.as_os_str()),
        ),
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let directory = root.path().join("session-version1");
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("events")).unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    write_json_atomic(
        &directory.join("manifest.json"),
        &SessionManifest {
            schema: SESSION_SCHEMA,
            id: "session-version1".to_owned(),
            provider: "claude".to_owned(),
            provider_path: provider.clone(),
            provider_version: VERSION.to_owned(),
            workspace,
            title: "Version check".to_owned(),
            model: None,
            effort: None,
            yolo: true,
            created_unix_ms: unix_ms(),
        },
    )
    .unwrap();
    write_private(&directory.join("initial-prompt.txt"), b"claude prompt").unwrap();
    update_status(&directory, SessionState::Launching, None, None).unwrap();
    turn::claim(&Store::open_unchecked(&directory), &[])
        .unwrap()
        .retain();
    Session {
        root: root.path().to_owned(),
        _root: root,
        directory,
        provider,
        probe_started,
        provider_started,
    }
}

/// The processes whose command line contains `path`, as `(pid, command line)`.
fn processes_carrying(path: &Path) -> Vec<(libc::pid_t, String)> {
    let output = Command::new("ps")
        .args(["-ww", "-A", "-o", "pid=,command="])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let needle = path.to_str().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| {
            let (pid, command) = line.trim_start().split_once(' ')?;
            Some((pid.parse().ok()?, command.to_owned()))
        })
        .collect()
}

const PROMPT: &str = "printf '%s\\n' '2.1.302 (Claude Code)'; exit 0";

impl Session {
    /// Writes the launch record through the launcher's own writer, for the current claim.
    fn begin(&self, request_deadline: Instant) {
        let token = turn::current_claim_token(&Reader::open_unchecked(&self.directory))
            .unwrap()
            .unwrap();
        launch::begin(
            &Store::open_unchecked(&self.directory),
            &token,
            request_deadline,
        )
        .unwrap();
    }

    /// Writes the launch record, then moves its deadline an hour away: the hang guard ends any
    /// call long before a correct one could reach that deadline, however it is scheduled.
    fn begin_unhurried(&self) {
        self.begin(Instant::now());
        self.set_deadline(unix_ms() + 3_600_000);
    }

    /// Replaces the recorded deadline, keeping the rest of the launch record.
    fn set_deadline(&self, deadline_unix_ms: u128) {
        let mut record = self.record().unwrap();
        record.deadline_unix_ms = deadline_unix_ms;
        Store::open_unchecked(&self.directory)
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
    }

    fn record(&self) -> Option<launch::Record> {
        launch::read(&Reader::open_unchecked(&self.directory)).unwrap()
    }

    fn label(&self) -> String {
        format!("{} --version", self.provider.display())
    }

    /// Runs `provision` under the guard.
    fn provision(&self) -> (Result<()>, Duration) {
        self.provision_at(None)
    }

    /// Runs `provision` on a worker thread under the guard, with the wall clock pinned at
    /// `pinned` on that thread when given. A call still blocked at the guard has every process
    /// carrying this session's directory killed, so the worker returns, and the test fails.
    fn provision_at(&self, pinned: Option<u128>) -> (Result<()>, Duration) {
        let (sender, receiver) = mpsc::channel();
        let directory = self.directory.clone();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            let _clock = pinned.map(PinnedClock::at);
            let _ = sender.send(provision(&directory));
        });
        match receiver.recv_timeout(GUARD) {
            Ok(result) => {
                let elapsed = started.elapsed();
                worker.join().unwrap();
                (result, elapsed)
            }
            Err(_) => {
                for (pid, _) in processes_carrying(&self.root) {
                    // SAFETY: kill(2) on a process whose command line names this test's
                    // temporary directory.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                let late = receiver.recv_timeout(GUARD);
                panic!(
                    "provision was still running after {GUARD:?}; after its probe was killed it returned {late:?}"
                );
            }
        }
    }

    /// Settles the session as the wrapper does after `provision` returned.
    fn finalize(&self, result: &Result<()>) {
        launch::finalize(&Store::open_unchecked(&self.directory), result).unwrap();
    }
}

fn message(result: &Result<()>) -> String {
    format!("{:#}", result.as_ref().unwrap_err())
}

// Characterization: a probe that answers a supported version at once lets the wrapper start
// the provider, with or without a launch record, and the launch settles the same way.
#[test]
fn wrapper_version_check_passes_a_prompt_supported_answer_with_and_without_a_launch_record() {
    for recorded in [false, true] {
        let session = session(PROMPT);
        if recorded {
            session.begin_unhurried();
        }
        let (result, _) = session.provision();
        session.finalize(&result);
        result.unwrap_or_else(|error| panic!("recorded={recorded}: {error:#}"));
        assert!(session.probe_started.exists(), "recorded={recorded}");
        assert!(session.provider_started.exists(), "recorded={recorded}");
        let status = Reader::open_unchecked(&session.directory).status().unwrap();
        assert_eq!(status.state, SessionState::Exited, "recorded={recorded}");
        assert_eq!(status.exit_code, Some(0), "recorded={recorded}");
        assert_eq!(
            session.record().map(|record| record.phase),
            recorded.then_some(Phase::Spawned)
        );
    }
}

// Characterization: a probe that answers but fails the gate refuses with the gate's own text
// and kind, before the provider exists, with or without a launch record.
#[test]
fn wrapper_version_check_keeps_its_refusals_with_and_without_a_launch_record() {
    let too_old = format!(
        "claude is too old: found \"0.0.1 (Claude Code)\", require >= {}",
        FirstPartyCli::Claude.minimum_version()
    );
    for (answer, unsupported, expected) in [
        (
            "printf '%s\\n' '0.0.1 (Claude Code)'; exit 0",
            true,
            too_old,
        ),
        (
            "printf '%s\\n' 'development build'; exit 0",
            false,
            "could not parse a semantic version from \"development build\"".to_owned(),
        ),
        (
            "exit 7",
            false,
            "--version exited with exit status: 7".to_owned(),
        ),
    ] {
        for recorded in [false, true] {
            let session = session(answer);
            if recorded {
                session.begin_unhurried();
            }
            let (result, _) = session.provision();
            let expected = if expected.starts_with("--version") {
                format!("{} {expected}", session.provider.display())
            } else {
                expected.clone()
            };
            assert_eq!(message(&result), expected, "recorded={recorded}");
            assert_eq!(
                classify(result.as_ref().unwrap_err()) == Some(FailureKind::Unsupported),
                unsupported,
                "recorded={recorded}: {expected}"
            );
            assert!(session.probe_started.exists());
            assert!(!session.provider_started.exists());
            assert_eq!(
                session.record().map(|record| record.phase),
                recorded.then_some(Phase::Pending)
            );
        }
    }
}

// Characterization: without a launch record a probe that cannot start keeps the unbounded
// executor's text.
#[test]
fn wrapper_version_check_without_a_launch_record_keeps_the_unbounded_start_failure_text() {
    let session = session(PROMPT);
    fs::remove_file(&session.provider).unwrap();
    let (result, _) = session.provision();
    assert_eq!(
        message(&result),
        format!(
            "failed to query {}: No such file or directory (os error 2)",
            session.label()
        )
    );
    assert_eq!(classify(result.as_ref().unwrap_err()), None);
}

// The bound `provision` hands to the check: the launch record's deadline plus the grace,
// converted once against the pinned wall clock, and bracketed by the monotonic readings around
// the call; none without a launch record or with one that cannot be read. The supplied check
// stops `provision` there, and its error comes back unchanged. No process is started.
#[test]
fn provision_hands_the_check_the_launch_deadline_plus_the_grace() {
    let handed = |session: &Session| {
        let mut captured = None;
        let before = Instant::now();
        let result = provision_with(&session.directory, |provider, path, bound| {
            captured = Some((provider, path.to_owned(), bound));
            Err(anyhow::anyhow!("captured"))
        });
        let after = Instant::now();
        assert_eq!(format!("{:#}", result.unwrap_err()), "captured");
        let (provider, path, bound) = captured.expect("provision did not reach the check");
        assert_eq!(provider, FirstPartyCli::Claude);
        assert_eq!(path, session.provider);
        (before, bound, after)
    };
    const NOW: u128 = 1_000_000_000_000;
    for (deadline, remaining_ms) in [
        (NOW + 500, 2_500),
        // A deadline already past still leaves the rest of the grace.
        (NOW - 500, 1_500),
        // A deadline older than the grace is now.
        (NOW - 5_000, 0),
    ] {
        let session = session(PROMPT);
        session.begin_unhurried();
        session.set_deadline(deadline);
        let _clock = PinnedClock::at(NOW);
        let (before, bound, after) = handed(&session);
        let bound = bound.unwrap_or_else(|| panic!("deadline {deadline}: no bound"));
        let remaining = Duration::from_millis(remaining_ms);
        assert!(
            before + remaining <= bound && bound <= after + remaining,
            "deadline {deadline}: {:?} after the call began",
            bound.saturating_duration_since(before)
        );
    }
    let _clock = PinnedClock::at(NOW);
    let session = session(PROMPT);
    assert_eq!(handed(&session).1, None, "no launch record");
    session.begin_unhurried();
    fs::write(
        Reader::open_unchecked(&session.directory)
            .record(CoreRecord::Launch)
            .path(),
        b"{",
    )
    .unwrap();
    assert_eq!(handed(&session).1, None, "an unreadable launch record");
}

// A probe that would hang cannot keep `provision`, and is no longer running when it returns.
// The wall clock is pinned on the thread that runs `provision`, with the recorded deadline
// 400 ms after it, so the check is bounded 2.4 s after its conversion however the test was
// scheduled before it. Either timeout of the bounded executor is correct: a stall between the
// conversion and the spawn only turns `timed out` into `timed out before it started`. Without
// the bound neither occurs and the call hangs until the guard.
// The hung probe is the system `sleep`, which the stub `exec`s through a link in this test's
// directory, using parameter expansion so that no other process starts first; its command line
// therefore carries that unique path. One process listing after `provision` returns shows no
// running process carrying it. A killed but unreaped child would not be matched: whether the
// executor reaped its child is the executor's own step and is not asserted here; the wrapper
// exits right after a failed `provision`, so such a child could not outlive it.
#[test]
fn a_probe_that_would_hang_ends_with_the_bounded_timeout_and_leaves_no_running_process() {
    let session = session("exec \"${0%/*}/bin/sleep\" 120");
    let system_sleep = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists())
        .expect("a system sleep");
    fs::create_dir(session.root.join("bin")).unwrap();
    std::os::unix::fs::symlink(system_sleep, session.root.join("bin/sleep")).unwrap();
    let pinned = unix_ms();
    session.begin_unhurried();
    session.set_deadline(pinned + RECORDED_BUDGET.as_millis());
    let (result, _) = session.provision_at(Some(pinned));
    let message = message(&result);
    assert!(
        [
            format!("{} timed out", session.label()),
            format!("{} timed out before it started", session.label()),
        ]
        .contains(&message),
        "{message}"
    );
    assert_eq!(
        classify(result.as_ref().unwrap_err()),
        Some(FailureKind::TimedOut)
    );
    assert_eq!(processes_carrying(&session.root), Vec::new());
    assert!(!session.provider_started.exists());
    session.finalize(&result);
    let reader = Reader::open_unchecked(&session.directory);
    let status = reader.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(message.as_str()));
    assert_eq!(session.record().unwrap().phase, Phase::Pending);
    assert_eq!(turn::current_claim_token(&reader).unwrap(), None);
}

// Without a launch record the check is not bounded: a probe that answers after longer than
// the recorded budget of the other tests still passes, and the provider starts.
#[test]
fn wrapper_version_check_without_a_launch_record_waits_for_a_late_answer() {
    let session = session("sleep 0.8; printf '%s\\n' '2.1.302 (Claude Code)'; exit 0");
    let (result, elapsed) = session.provision();
    session.finalize(&result);
    result.unwrap();
    assert!(elapsed >= Duration::from_millis(800), "{elapsed:?}");
    assert!(elapsed > RECORDED_BUDGET);
    assert!(session.provider_started.exists());
    assert_eq!(session.record().map(|record| record.phase), None);
}

// A launch deadline older than its grace refuses before the probe starts. The launch then
// settles as the spawn fence's refusal does: failed, unspawned, the claim released.
#[test]
fn wrapper_version_check_refuses_a_launch_deadline_past_its_grace_before_the_probe_starts() {
    let session = session(PROMPT);
    session.begin(Instant::now());
    // A minute older than the grace: a delay only makes it older.
    session.set_deadline(unix_ms() - GRACE.as_millis() - 60_000);
    let (result, _) = session.provision();
    assert!(result.is_err());
    session.finalize(&result);
    let reader = Reader::open_unchecked(&session.directory);
    let status = reader.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.exit_code, Some(1));
    let record = session.record().unwrap();
    assert_eq!(record.phase, Phase::Pending);
    assert_eq!(turn::current_claim_token(&reader).unwrap(), None);
    assert_eq!(
        launch::diagnostic(Some(&record), &status, None).map(|(verdict, _)| verdict),
        Some(Verdict::Failed)
    );
    assert!(!session.provider_started.exists());
    let refusal = format!("{} timed out before it started", session.label());
    assert_eq!(message(&result), refusal);
    assert_eq!(status.error.as_deref(), Some(refusal.as_str()));
    assert_eq!(
        classify(result.as_ref().unwrap_err()),
        Some(FailureKind::TimedOut)
    );
    assert!(
        !session.probe_started.exists(),
        "the probe started after the deadline"
    );
}

// With a launch record the check runs through the bounded executor, so a probe that cannot
// start reports that executor's text.
#[test]
fn wrapper_version_check_with_a_launch_record_reports_the_bounded_start_failure_text() {
    let session = session(PROMPT);
    session.begin_unhurried();
    fs::remove_file(&session.provider).unwrap();
    let (result, _) = session.provision();
    assert_eq!(
        message(&result),
        format!(
            "failed to start {}: No such file or directory (os error 2)",
            session.label()
        )
    );
    assert_eq!(classify(result.as_ref().unwrap_err()), None);
    assert_eq!(session.record().unwrap().phase, Phase::Pending);
}

// Characterization: the largest deadline a launch record can be read with is handled: the check
// succeeds and the launch spawns. Whether that deadline gives a bound depends on the platform's
// monotonic clock; the launch module's conversion test covers it.
#[test]
fn wrapper_version_check_handles_the_largest_recordable_launch_deadline() {
    let session = session(PROMPT);
    session.begin_unhurried();
    let mut record = session.record().unwrap();
    record.deadline_unix_ms = u128::from(u64::MAX);
    Store::open_unchecked(&session.directory)
        .record(CoreRecord::Launch)
        .write_json(&record)
        .unwrap();
    assert_eq!(
        session.record().unwrap().deadline_unix_ms,
        u128::from(u64::MAX)
    );
    let (result, _) = session.provision();
    session.finalize(&result);
    result.unwrap();
    assert!(session.provider_started.exists());
    assert_eq!(session.record().unwrap().phase, Phase::Spawned);
}

// Characterization: a launch record that cannot be read does not bound or refuse the check; the
// spawn fence reads it under the lifecycle lock and refuses, before the provider exists.
#[test]
fn wrapper_version_check_leaves_an_unreadable_launch_record_to_the_spawn_fence() {
    let session = session(PROMPT);
    session.begin_unhurried();
    let path = Reader::open_unchecked(&session.directory)
        .record(CoreRecord::Launch)
        .path()
        .to_owned();
    fs::write(&path, b"{").unwrap();
    let (result, _) = session.provision();
    let message = message(&result);
    assert!(
        message.starts_with(&format!(
            "failed to start claude at {}: invalid launch receipt",
            session.provider.display()
        )),
        "{message}"
    );
    assert!(session.probe_started.exists());
    assert!(!session.provider_started.exists());
}

// Characterization of the settlement the grace relies on, with no timing: the launcher's own
// deadline wait has failed the launch; the wrapper's settlement of the timeout its bounded check
// returns, built as the executor builds it, then leaves that failure and the released claim
// exactly as recorded. That the wrapper's bound comes after the launcher's deadline is the
// evidence of the conversion test and of the bound-handoff test.
#[test]
fn a_wrapper_settling_its_timeout_after_the_launchers_deadline_leaves_the_launchers_failure() {
    let session = session(PROMPT);
    session.begin_unhurried();
    let surface: crate::native::terminal::TerminalSession = serde_json::from_value(
        serde_json::json!({"terminal": "windows-console", "session_id": "surface"}),
    )
    .unwrap();
    let error = launch::wait(
        &Store::open_unchecked(&session.directory),
        &surface,
        Instant::now(),
    )
    .unwrap_err();
    const LAUNCH_DEADLINE: &str = "provider launch timed out before startup was confirmed";
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert_eq!(classify(&error), Some(FailureKind::TimedOut));
    let reader = Reader::open_unchecked(&session.directory);
    let status_path = session.directory.join("status.json");
    let recorded = fs::read(&status_path).unwrap();
    let status = reader.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(LAUNCH_DEADLINE));
    assert_eq!(turn::current_claim_token(&reader).unwrap(), None);
    assert_eq!(session.record().unwrap().phase, Phase::Pending);

    // `provision` returns the executor's error unchanged; the hung-probe test accepts that text.
    let result: Result<()> = Err(anyhow::Error::new(TimedOut::new(format!(
        "{} timed out",
        session.label()
    ))));
    assert_eq!(
        classify(result.as_ref().unwrap_err()),
        Some(FailureKind::TimedOut)
    );
    session.finalize(&result);
    assert_eq!(fs::read(&status_path).unwrap(), recorded);
    assert_eq!(turn::current_claim_token(&reader).unwrap(), None);
    assert_eq!(session.record().unwrap().phase, Phase::Pending);
    assert!(!session.probe_started.exists());
    assert!(!session.provider_started.exists());
}
