//! The launcher's wait for a confirmed launch: each way it ends, on a simulated clock and a
//! scripted surface probe, with real records and locks. No test waits on the wall clock.
use super::*;
use std::cell::Cell;
use std::rc::Rc;

fn fixture() -> (tempfile::TempDir, Store, String) {
    test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT)
}

fn ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

// Each sleep advances the shared instant and is recorded, in milliseconds.
struct Simulated {
    now: Rc<Cell<Instant>>,
    sleeps: Vec<u64>,
}

impl Clock for Simulated {
    fn now(&mut self) -> Instant {
        self.now.get()
    }

    fn sleep(&mut self, duration: Duration) {
        self.sleeps.push(ms(duration));
        self.now.set(self.now.get() + duration);
    }
}

// Each probe is recorded as (its time since the start, its bound) in milliseconds, then
// answered by the script from its time. The script may change the records while it runs, as
// the launch's own processes and readers can while a probe waits on a core.
struct Probes<F> {
    start: Instant,
    now: Rc<Cell<Instant>>,
    calls: Vec<(u64, u64)>,
    answer: F,
}

impl<F: FnMut(u64) -> Result<terminal::LaunchSurface>> SurfacePresence for Probes<F> {
    fn present(&mut self, timeout: Duration) -> Result<terminal::LaunchSurface> {
        let at = ms(self.now.get() - self.start);
        self.calls.push((at, ms(timeout)));
        (self.answer)(at)
    }
}

struct Run {
    result: Result<()>,
    sleeps: Vec<u64>,
    probes: Vec<(u64, u64)>,
}

/// Runs the wait with its deadline `deadline_ms` after its start, for a surface that is
/// present or absent.
fn run(store: &Store, deadline_ms: u64, mut answer: impl FnMut(u64) -> Result<bool>) -> Run {
    run_probed(store, deadline_ms, move |at| {
        answer(at).map(terminal::LaunchSurface::from_presence)
    })
}

/// Runs the wait of the launch whose Claim the fixture installed, with its deadline
/// `deadline_ms` after its start.
fn run_probed(
    store: &Store,
    deadline_ms: u64,
    answer: impl FnMut(u64) -> Result<terminal::LaunchSurface>,
) -> Run {
    run_stepping(store, deadline_ms, answer, |_| {})
}

// A simulated clock whose every sleep first runs `step` with the time since the start: what
// another process does while the wait sleeps.
struct Stepping<F> {
    start: Instant,
    now: Rc<Cell<Instant>>,
    sleeps: Vec<u64>,
    step: F,
}

impl<F: FnMut(u64)> Clock for Stepping<F> {
    fn now(&mut self) -> Instant {
        self.now.get()
    }

    fn sleep(&mut self, duration: Duration) {
        (self.step)(ms(self.now.get() - self.start));
        self.sleeps.push(ms(duration));
        self.now.set(self.now.get() + duration);
    }
}

/// `run_probed`, with `step` run at every sleep.
fn run_stepping(
    store: &Store,
    deadline_ms: u64,
    answer: impl FnMut(u64) -> Result<terminal::LaunchSurface>,
    step: impl FnMut(u64),
) -> Run {
    let token = turn::current_claim_token(store).unwrap().unwrap();
    let start = Instant::now();
    let now = Rc::new(Cell::new(start));
    let mut clock = Stepping {
        start,
        now: Rc::clone(&now),
        sleeps: Vec::new(),
        step,
    };
    let mut presence = Probes {
        start,
        now,
        calls: Vec::new(),
        answer,
    };
    let result = wait_with(
        store,
        &token,
        start + Duration::from_millis(deadline_ms),
        &mut clock,
        &mut presence,
    );
    Run {
        result,
        sleeps: clock.sleeps,
        probes: presence.calls,
    }
}

fn unprobed(_: u64) -> Result<bool> {
    panic!("the surface is not probed")
}

fn spawned(store: &Store) {
    let mut record = read(store).unwrap().unwrap();
    record.phase = Phase::Spawned;
    store
        .record(CoreRecord::Launch)
        .write_json(&record)
        .unwrap();
}

fn claim_is(store: &Store, token: &str) -> bool {
    turn::current_claim_token(store).unwrap().as_deref() == Some(token)
}

#[test]
fn wait_returns_for_a_confirmed_launch_with_and_without_the_lock() {
    for held in [false, true] {
        let (_dir, store, _) = fixture();
        spawned(&store);
        let lock = held.then(|| store.lock().unwrap());
        let run = run(&store, 20_000, unprobed);
        drop(lock);
        run.result.unwrap();
        assert!(run.sleeps.is_empty(), "held={held}");
    }
}

// A launch confirmed while the wait sleeps ends it at the next poll; one confirmed while a
// probe that finds the surface gone runs ends it too, at the locked re-read.
#[test]
fn a_launch_confirmed_during_the_wait_ends_it() {
    for disappeared in [false, true] {
        let (_dir, store, _) = fixture();
        let run = run(&store, 20_000, |_| {
            spawned(&store);
            Ok(!disappeared)
        });
        run.result.unwrap();
        assert_eq!(run.probes, [(1000, 2000)], "disappeared={disappeared}");
        // A present surface leaves the confirmation to the next poll.
        let polls = if disappeared { 10 } else { 11 };
        assert_eq!(run.sleeps, vec![100; polls], "disappeared={disappeared}");
        assert_eq!(store.status().unwrap().state, SessionState::Launching);
    }
}

#[test]
fn wait_names_how_an_ended_launch_ended() {
    let mut failures = Vec::new();
    for (case, held, expected) in [
        ("failed with its error", false, "earlier failure"),
        ("failed without an error", false, "launch entered failed"),
        ("tombstone while launching", false, "launch entered closed"),
        ("failed with its error, lock held", true, "earlier failure"),
        (
            "failed without an error, lock held",
            true,
            "launch entered failed",
        ),
    ] {
        let (_dir, store, token) = fixture();
        match case {
            "tombstone while launching" => store.write_closed(&store.status().unwrap()).unwrap(),
            _ => update_status(
                store.directory(),
                SessionState::Failed,
                None,
                case.starts_with("failed with its error")
                    .then(|| "earlier failure".to_owned()),
            )
            .unwrap(),
        }
        let before = store.status().unwrap();
        let lock = held.then(|| store.lock().unwrap());
        let run = run(&store, 20_000, unprobed);
        drop(lock);
        let error = run.result.unwrap_err();
        let after = store.status().unwrap();
        if format!("{error:#}") != expected
            || error.downcast_ref::<TimedOut>().is_some()
            || !run.sleeps.is_empty()
            || after.state != before.state
            || after.error != before.error
            || !claim_is(&store, &token)
        {
            failures.push(format!("{case}: {error:#}, {after:?}, {:?}", run.sleeps));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// A launch that ends while a probe that finds the surface gone runs keeps its own outcome.
#[test]
fn a_launch_ended_during_a_probe_keeps_its_own_error() {
    for (error, expected) in [
        (Some("earlier failure"), "earlier failure"),
        (None, "launch has ended"),
    ] {
        let (_dir, store, token) = fixture();
        let run = run(&store, 20_000, |_| {
            update_status(
                store.directory(),
                SessionState::Failed,
                None,
                error.map(str::to_owned),
            )
            .unwrap();
            Ok(false)
        });
        let actual = run.result.unwrap_err();
        assert_eq!(format!("{actual:#}"), expected);
        assert!(actual.downcast_ref::<TimedOut>().is_none());
        assert_eq!(store.status().unwrap().error.as_deref(), error);
        assert!(claim_is(&store, &token));
    }
}

#[test]
fn a_failed_host_ends_the_wait_with_the_failure_it_records() {
    let (_dir, store, token) = fixture();
    host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
    host::fail(
        &store,
        &token,
        None,
        host::Failure {
            reason: host::FailureReason::IdentityUnavailable,
            exit_code: None,
            signal: None,
        },
        0,
        None,
        None,
    )
    .unwrap();
    let run = run(&store, 20_000, unprobed);
    let error = run.result.unwrap_err();
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(Some(format!("{error:#}")), status.error);
    assert!(error.downcast_ref::<TimedOut>().is_none());
    assert!(run.sleeps.is_empty());
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

// Full 100 ms polls, then the remainder; no probe is due before one second.
#[test]
fn the_deadline_fails_the_launch_after_its_last_partial_poll() {
    let (_dir, store, _) = fixture();
    let run = run(&store, 250, unprobed);
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert!(error.downcast_ref::<TimedOut>().is_some());
    assert_eq!(run.sleeps, [100, 100, 50]);
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(LAUNCH_DEADLINE)
    );
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

#[test]
fn surface_probes_are_due_every_second_within_two_seconds_and_the_deadline() {
    let (_dir, store, _) = fixture();
    let run = run(&store, 3500, |_| Ok(true));
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert_eq!(run.probes, [(1000, 2000), (2000, 1500), (3000, 500)]);
    assert_eq!(run.sleeps, [100; 35]);
}

// The case observed on 2026-11-26 (Windows, hc 0.1.21): a launch host that refuses its session
// directory leaves only its line in the launch log, no Record, and its finished Herdr terminal
// still reads as present, so the wait ends only at the launch deadline, with the generic
// timeout. A refusal the launcher can know beforehand no longer reaches the wait; one that
// only the host can know still ends this way.
#[test]
fn a_directory_refusal_in_the_log_alone_ends_the_wait_at_the_deadline() {
    let (_dir, store, token) = fixture();
    log(
        &store,
        "herdr_host_refused: Herdr launch directory is not private to the current user: session directory owner differs from current user",
    );
    let run = run(&store, START_TIMEOUT.as_millis() as u64, |_| Ok(true));
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert!(error.downcast_ref::<TimedOut>().is_some());
    assert_eq!(
        run.sleeps.iter().sum::<u64>(),
        START_TIMEOUT.as_millis() as u64
    );
    assert_eq!(run.probes.len(), 29);
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(LAUNCH_DEADLINE)
    );
    assert!(!claim_is(&store, &token));
}

// A presence check that cannot inspect the surface does not prove its absence: the wait goes
// on past its first probe, due one second after it starts, to its deadline.
#[test]
fn surface_probe_error_does_not_end_the_wait() {
    let (_dir, store, _) = fixture();
    let run = run(&store, 1150, |_| bail!("inspection denied"));
    let error = run.result.unwrap_err();
    assert_eq!(run.probes, [(1000, 150)]);
    let mut sleeps = vec![100; 11];
    sleeps.push(50);
    assert_eq!(run.sleeps, sleeps);
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert!(error.downcast_ref::<TimedOut>().is_some());
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(LAUNCH_DEADLINE)
    );
}

#[test]
fn surface_disappeared_fails_the_launch_at_a_due_probe() {
    let (_dir, store, _) = fixture();
    let run = run(&store, 2500, |at| Ok(at < 2000));
    let error = run.result.unwrap_err();
    assert_eq!(run.probes, [(1000, 1500), (2000, 500)]);
    assert_eq!(run.sleeps, [100; 20]);
    assert_eq!(format!("{error:#}"), SURFACE_DISAPPEARED);
    assert!(error.downcast_ref::<TimedOut>().is_none());
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(SURFACE_DISAPPEARED));
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

// While another holder keeps the lifecycle lock, neither reason is recorded and the claim
// is retained; only the deadline's own sentence carries the timeout hint.
#[test]
fn a_held_lock_retains_the_claim_for_either_reason() {
    for (deadline_ms, reason, timed_out) in [
        (20_000, SURFACE_DISAPPEARED, false),
        (250, LAUNCH_DEADLINE, true),
    ] {
        let (_dir, store, token) = fixture();
        let before = store.status().unwrap();
        let lock = store.lock().unwrap();
        let run = run(&store, deadline_ms, |_| Ok(false));
        drop(lock);
        let error = run.result.unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!(
                "{reason}; startup is still holding the lifecycle lock, execution is uncertain; the claim is retained"
            )
        );
        assert_eq!(error.downcast_ref::<TimedOut>().is_some(), timed_out);
        let after = store.status().unwrap();
        assert_eq!((after.state, after.error), (before.state, before.error));
        assert!(claim_is(&store, &token));
    }
}

// The wait with its deadline `deadline_ms` after its start, probing `surface` through the
// installed terminal adapters, as the launcher does. Like the scripted Herdr core it serves,
// it exists where TAB runs Herdr.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn run_on_surface(store: &Store, deadline_ms: u64, surface: &terminal::TerminalSession) -> Run {
    let token = turn::current_claim_token(store).unwrap().unwrap();
    let start = Instant::now();
    let mut clock = Simulated {
        now: Rc::new(Cell::new(start)),
        sleeps: Vec::new(),
    };
    let mut presence = TerminalSurface {
        directory: store.directory(),
        surface,
    };
    let result = wait_with(
        store,
        &token,
        start + Duration::from_millis(deadline_ms),
        &mut clock,
        &mut presence,
    );
    Run {
        result,
        sleeps: clock.sleeps,
        probes: Vec::new(),
    }
}

// The owner's decision of 2026-11-26 (option A): a Herdr terminal that its verified core
// reports finished before startup was confirmed ends the wait at the next probe, as a tmux
// surface that disappears does, instead of at the launch deadline. The Session records a fixed
// reason; the error returned to the caller adds the core's report (the terminal, its exit code
// when given) and the launch log's path. A refusal line in that log changes nothing in either.
// The launch ends through the settlement the deadline uses: failed, its Claim released, its
// receipt pending and its surface handle kept, so that close-session removes the finished
// terminal.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_herdr_terminal_that_finished_before_startup_ends_the_wait_at_the_next_probe() {
    use crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore;
    for (exit_code, refusal) in [(Some(1), true), (None, false)] {
        let core = ScriptedHerdrCore::start();
        let (_dir, store, token) = fixture();
        let surface = core.hold_launch(
            store.directory(),
            "finished",
            exit_code.map_or(
                serde_json::json!({}),
                |code| serde_json::json!({"exit_code": code}),
            ),
        );
        store.write_terminal(&surface).unwrap();
        if refusal {
            log(
                &store,
                "herdr_host_refused: Herdr launch directory is not private to the current user",
            );
        }
        let run = run_on_surface(&store, START_TIMEOUT.as_millis() as u64, &surface);
        let error = run.result.unwrap_err();
        let expected = format!(
            "terminal surface finished before provider launch was confirmed (Herdr terminal t7 {}); see the launch log {}",
            exit_code.map_or("finished".to_owned(), |code| format!(
                "exited with code {code}"
            )),
            store.directory().join(LOG).display()
        );
        assert_eq!(format!("{error:#}"), expected);
        assert!(error.downcast_ref::<TimedOut>().is_none());
        assert_eq!(run.sleeps, [100; 10], "{exit_code:?}");
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(status.error.as_deref(), Some(SURFACE_FINISHED));
        assert!(!claim_is(&store, &token));
        assert_eq!(read(&store).unwrap().unwrap().phase, Phase::Pending);
        assert_eq!(store.terminal().unwrap(), surface);
        assert!(core.holds_launch());
        // `close-session` then removes the finished terminal and closes the session.
        let id = store.directory().file_name().unwrap().to_str().unwrap();
        crate::native::close::close_session_surface(
            store.directory(),
            id,
            None,
            crate::native::session::close::budget::close_deadline(),
        )
        .unwrap();
        assert!(!core.holds_launch());
        assert_eq!(store.status().unwrap().state, SessionState::Closed);
    }
}

// What already decides the launch keeps deciding it when the terminal has finished: a
// confirmed startup ends the wait as confirmed, and a launch-host Record (#409) names the
// failure it records.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn a_confirmed_launch_or_a_host_record_wins_over_a_finished_herdr_terminal() {
    use crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore;
    let core = ScriptedHerdrCore::start();
    let (_dir, store, _) = fixture();
    let surface = core.hold_launch(store.directory(), "finished", serde_json::json!({}));
    store.write_terminal(&surface).unwrap();
    spawned(&store);
    run_on_surface(&store, 20_000, &surface).result.unwrap();

    let core = ScriptedHerdrCore::start();
    let (_dir, store, token) = fixture();
    let surface = core.hold_launch(store.directory(), "finished", serde_json::json!({}));
    store.write_terminal(&surface).unwrap();
    host::fail_bound_herdr(
        &store,
        &token,
        &surface,
        host::FailureReason::InputFlushFailed,
        0,
    )
    .unwrap();
    let error = run_on_surface(&store, 20_000, &surface).result.unwrap_err();
    let status = store.status().unwrap();
    assert_eq!(Some(format!("{error:#}")), status.error);
    assert!(
        format!("{error:#}").contains("(input_flush_failed)"),
        "{error:#}"
    );
}

// What a finished terminal brings forward is the outcome the launch deadline would give the
// same launch, so it acts only when that outcome is certain. These tests change the records
// while the probe that finds the terminal finished is running, as the launch's own processes
// and readers can while the probe waits on the core.
fn finished() -> Result<terminal::LaunchSurface> {
    Ok(terminal::LaunchSurface::Finished(
        "Herdr terminal t7 exited with code 1".into(),
    ))
}

fn finished_error(store: &Store) -> String {
    format!(
        "{SURFACE_FINISHED} (Herdr terminal t7 exited with code 1); see the launch log {}",
        store.directory().join(LOG).display()
    )
}

// A reader of the lifecycle lock (`lock_shared`), held from `take` until `release`.
#[derive(Default)]
struct LockReader(std::cell::RefCell<Option<std::fs::File>>);

impl LockReader {
    fn take(&self, store: &Store) {
        *self.0.borrow_mut() = Some(store.lock_shared().unwrap().unwrap());
    }

    fn release(&self) {
        self.0.borrow_mut().take();
    }
}

// The Session records only the fixed reason, which names no path: a state root whose path
// contains the legacy residual-surface text does not make the failure report a residual
// surface. The error returned to the caller names the terminal and the launch log.
#[test]
fn a_finished_terminal_records_a_fixed_reason_and_returns_its_details() {
    let parent = tempfile::Builder::new()
        .prefix(&format!("{RESIDUAL_SURFACE_MARKER} "))
        .tempdir()
        .unwrap();
    let (_dir, store, token) =
        test_support::pending_launch("session-", parent.path(), START_TIMEOUT);
    let run = run_probed(&store, 20_000, |_| finished());
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), finished_error(&store));
    assert!(error.downcast_ref::<TimedOut>().is_none());
    assert_eq!(run.probes, [(1000, 2000)]);
    assert_eq!(run.sleeps, [100; 10]);
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(SURFACE_FINISHED));
    assert_eq!(status.residual_surface(), None);
    assert!(!claim_is(&store, &token));
}

// A reader that holds the lifecycle lock when the finished terminal is seen makes that probe
// decide nothing: the wait goes on as for a present surface, and a later probe that finds the
// lock free ends the launch.
#[test]
fn a_reader_holding_the_lock_leaves_a_finished_terminal_to_a_later_probe() {
    let (_dir, store, token) = fixture();
    let reader = LockReader::default();
    let run = run_probed(&store, 20_000, |at| {
        if at == 1000 {
            reader.take(&store);
        } else {
            reader.release();
        }
        finished()
    });
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), finished_error(&store));
    assert_eq!(run.probes, [(1000, 2000), (2000, 2000)]);
    assert_eq!(run.sleeps, [100; 20]);
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(SURFACE_FINISHED));
    assert!(!claim_is(&store, &token));
}

// A confirmation published while the probe runs is the launch's outcome, whether or not a
// reader holds the lock as the probe returns: the next poll reads it, as for a present
// surface.
#[test]
fn a_confirmation_published_during_the_probe_wins_over_a_finished_terminal() {
    for held in [false, true] {
        let (_dir, store, token) = fixture();
        let reader = LockReader::default();
        let run = run_probed(&store, 20_000, |_| {
            spawned(&store);
            if held {
                reader.take(&store);
            }
            finished()
        });
        reader.release();
        run.result.unwrap();
        assert_eq!(run.sleeps, [100; 11], "held={held}");
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Launching, "held={held}");
        assert!(claim_is(&store, &token), "held={held}");
    }
}

// A provider that spawned, exited and was settled by its wrapper while the probe ran has
// ended the launch: the wait returns the provider's error, never success.
#[test]
fn a_provider_failure_published_during_the_probe_wins_over_a_finished_terminal() {
    let (_dir, store, _) = fixture();
    let run = run_probed(&store, 20_000, |_| {
        spawned(&store);
        finalize(&store, &Err(anyhow::anyhow!("provider exited with code 2"))).unwrap();
        finished()
    });
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), "provider exited with code 2");
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some("provider exited with code 2"));
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

// A launch-host Record published while the probe runs names the failure, even when a reader
// holds the lock as the probe returns: the first poll that finds the lock free settles it.
#[test]
fn a_host_record_published_during_the_probe_wins_over_a_finished_terminal() {
    let (_dir, store, token) = fixture();
    host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
    let reader = LockReader::default();
    let run = run_probed(&store, 20_000, |at| {
        if at == 1000 {
            host::fail(
                &store,
                &token,
                None,
                host::Failure {
                    reason: host::FailureReason::IdentityUnavailable,
                    exit_code: None,
                    signal: None,
                },
                0,
                None,
                None,
            )
            .unwrap();
            reader.take(&store);
        } else {
            reader.release();
        }
        finished()
    });
    let error = run.result.unwrap_err();
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(Some(format!("{error:#}")), status.error);
    assert!(
        format!("{error:#}").starts_with("terminal launch host failed"),
        "{error:#}"
    );
    assert!(
        !format!("{error:#}").contains(SURFACE_FINISHED),
        "{error:#}"
    );
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

// A wrapper that recorded `Spawning` may have started a provider that can still confirm
// (`confirmed`), so a finished terminal decides nothing for it: a confirmation that arrives
// later is the outcome, and without one the launch deadline settles it as before.
#[test]
fn a_spawning_receipt_is_left_to_its_confirmation_or_the_deadline() {
    let (_dir, store, token) = fixture();
    test_support::apply(&store, &[test_support::Change::ReceiptSpawning]);
    let run = run_probed(&store, 20_000, |at| {
        if at == 2000 {
            let id = store.directory().file_name().unwrap().to_str().unwrap();
            store
                .record(CoreRecord::ProviderProcess)
                .write_json(&crate::native::session::schema::ProviderProcessRecord {
                    schema: 1,
                    managed_session_id: id.to_owned(),
                    pid: 0,
                    windows_process_identity: None,
                    spawned_unix_ms: 1,
                })
                .unwrap();
            update_status(store.directory(), SessionState::Running, None, None).unwrap();
        }
        finished()
    });
    run.result.unwrap();
    assert_eq!(run.sleeps, [100; 21]);
    assert!(claim_is(&store, &token));

    let (_dir, store, token) = fixture();
    test_support::apply(&store, &[test_support::Change::ReceiptSpawning]);
    let run = run_probed(&store, 2500, |_| finished());
    let error = run.result.unwrap_err();
    let expected = format!(
        "{LAUNCH_DEADLINE}; provider spawn is uncertain; the claim is retained, do not resend"
    );
    assert_eq!(format!("{error:#}"), expected);
    assert_eq!(run.probes.len(), 2);
    assert_eq!(store.status().unwrap().error, Some(expected));
    assert!(claim_is(&store, &token));
}

// Only this launch's Claim lets a finished terminal decide. While the path holds another
// launch's records (a directory replaced for a moment), that probe writes nothing; once this
// launch's records are back, a later probe ends this launch.
#[test]
fn a_finished_terminal_never_settles_another_launch_at_its_path() {
    let (_dir, store, token) = fixture();
    let restored = Cell::new(false);
    let run = run_probed(&store, 20_000, |at| {
        if at == 1000 {
            test_support::apply(
                &store,
                &[
                    test_support::Change::ReceiptToken,
                    test_support::Change::ClaimOther,
                ],
            );
        } else if !restored.get() {
            let status = store.status().unwrap();
            assert_eq!(
                (status.state, status.error),
                (SessionState::Launching, None)
            );
            assert!(claim_is(&store, "successor"));
            assert_eq!(read(&store).unwrap().unwrap().claim_token, "successor");
            let mut record = read(&store).unwrap().unwrap();
            record.claim_token = token.clone();
            store
                .record(CoreRecord::Launch)
                .write_json(&record)
                .unwrap();
            let claim = store.directory().join(CoreRecord::TurnClaim.name());
            std::fs::remove_file(&claim).unwrap();
            crate::native::session::RecordStore::at(&claim)
                .write_private(token.as_bytes())
                .unwrap();
            restored.set(true);
        }
        finished()
    });
    assert!(restored.get());
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), finished_error(&store));
    assert_eq!(run.probes, [(1000, 2000), (2000, 2000)]);
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(SURFACE_FINISHED)
    );
    assert!(turn::current_claim_token(&store).unwrap().is_none());
}

// A refusal before any Surface waits out a lifecycle lock that another holder keeps only until
// the launch's own deadline; past it the held lock keeps the Claim and the records as they
// are, as for every failure to open a Surface, and the caller is given the refusal with that
// outcome.
#[test]
fn a_refusal_keeps_the_claim_once_its_launch_budget_is_spent_on_a_held_lock() {
    let (_dir, store, token) = fixture();
    let before = store.status().unwrap();
    let lock = store.lock().unwrap();
    let start = Instant::now();
    let mut clock = Simulated {
        now: Rc::new(Cell::new(start)),
        sleeps: Vec::new(),
    };
    let error = refused_before_surface_with(
        &store,
        &token,
        anyhow::anyhow!("the Herdr launch host would refuse the session directory /state/x"),
        "the Herdr launch host would refuse the session directory",
        start + Duration::from_millis(250),
        &mut clock,
    );
    drop(lock);
    assert_eq!(clock.sleeps, [100, 100, 50]);
    assert_eq!(
        format!("{error:#}"),
        "terminal launch failure handoff: terminal launch failed: the Herdr launch host would refuse the session directory; provider startup is in progress; the claim is retained: the Herdr launch host would refuse the session directory /state/x"
    );
    let after = store.status().unwrap();
    assert_eq!((after.state, after.error), (before.state, before.error));
    assert!(claim_is(&store, &token));
}

// The bar for the early decisions: facts about this launch's own records, read under the
// lifecycle lock at the moment of the decision. A receipt of another Claim is not this
// launch's pending receipt, even with this launch's Claim at the path: the finished terminal
// decides nothing, and the launch deadline settles the launch as before.
#[test]
fn a_receipt_of_another_claim_never_ends_the_wait_as_finished() {
    let (_dir, store, token) = fixture();
    let run = run_probed(&store, 2500, |at| {
        if at == 1000 {
            test_support::apply(&store, &[test_support::Change::ReceiptToken]);
        }
        finished()
    });
    let error = run.result.unwrap_err();
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert_eq!(run.probes.len(), 2);
    let status = store.status().unwrap();
    assert_eq!(
        (status.state, status.error),
        (SessionState::Launching, None)
    );
    assert!(claim_is(&store, &token));
}

// A launch-host Record that cannot be read is no evidence that the launch is pending: the
// finished terminal decides nothing, and when the Record becomes readable with a failure, the
// ordinary poll names that failure.
#[test]
fn an_unreadable_host_record_defers_a_finished_terminal() {
    let (_dir, store, token) = fixture();
    host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
    let path = store.directory().join(CoreRecord::LaunchHost.name());
    let original = std::fs::read(&path).unwrap();
    let run = run_probed(&store, 20_000, |at| {
        if at == 1000 {
            std::fs::write(&path, b"{").unwrap();
        } else if at == 2000 {
            assert_eq!(store.status().unwrap().state, SessionState::Launching);
            std::fs::write(&path, &original).unwrap();
            host::fail(
                &store,
                &token,
                None,
                host::Failure {
                    reason: host::FailureReason::IdentityUnavailable,
                    exit_code: None,
                    signal: None,
                },
                0,
                None,
                None,
            )
            .unwrap();
        }
        finished()
    });
    let error = run.result.unwrap_err();
    assert!(
        format!("{error:#}").starts_with("terminal launch host failed"),
        "{error:#}"
    );
    assert_eq!(Some(format!("{error:#}")), store.status().unwrap().error);
    assert_eq!(run.probes.len(), 2);
}

// A host failure that the finished probe finds recorded is the ordinary poll's to publish:
// the probe writes nothing. A reader that takes the lock right after the probe and keeps it
// through the deadline then leaves the Claim retained, as without the probe.
#[test]
fn a_host_failure_seen_by_the_finished_probe_is_left_to_the_ordinary_poll() {
    let (_dir, store, token) = fixture();
    host::initialize(&store, &token, terminal::TerminalKind::Iterm2).unwrap();
    let reader = LockReader::default();
    let run = run_stepping(
        &store,
        2500,
        |at| {
            if at == 1000 {
                host::fail(
                    &store,
                    &token,
                    None,
                    host::Failure {
                        reason: host::FailureReason::IdentityUnavailable,
                        exit_code: None,
                        signal: None,
                    },
                    0,
                    None,
                    None,
                )
                .unwrap();
            }
            finished()
        },
        |at| {
            if at == 1000 && reader.0.borrow().is_none() {
                reader.take(&store);
            }
        },
    );
    reader.release();
    let error = run.result.unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!(
            "{LAUNCH_DEADLINE}; startup is still holding the lifecycle lock, execution is uncertain; the claim is retained"
        )
    );
    let status = store.status().unwrap();
    assert_eq!(
        (status.state, status.error),
        (SessionState::Launching, None)
    );
    assert!(claim_is(&store, &token));
}

// A probe that the core answers after the launch deadline decides nothing: the deadline path
// settles the launch, as without the probe.
#[test]
fn a_finished_reply_after_the_deadline_is_left_to_the_deadline() {
    struct LateReply(Rc<Cell<Instant>>);
    impl SurfacePresence for LateReply {
        fn present(&mut self, _: Duration) -> Result<terminal::LaunchSurface> {
            self.0.set(self.0.get() + Duration::from_millis(600));
            finished()
        }
    }
    let (_dir, store, token) = fixture();
    let start = Instant::now();
    let now = Rc::new(Cell::new(start));
    let mut clock = Simulated {
        now: Rc::clone(&now),
        sleeps: Vec::new(),
    };
    let error = wait_with(
        &store,
        &token,
        start + Duration::from_millis(1500),
        &mut clock,
        &mut LateReply(now),
    )
    .unwrap_err();
    assert_eq!(format!("{error:#}"), LAUNCH_DEADLINE);
    assert!(error.downcast_ref::<TimedOut>().is_some());
    assert_eq!(
        store.status().unwrap().error.as_deref(),
        Some(LAUNCH_DEADLINE)
    );
}

// A refusal whose lifecycle lock is freed only after the launch deadline never settles: the
// budget is spent, and the held lock's outcome stands.
#[test]
fn a_refusal_never_settles_after_its_launch_budget() {
    let (_dir, store, token) = fixture();
    let lock = std::cell::RefCell::new(Some(store.lock().unwrap()));
    let start = Instant::now();
    let mut clock = Stepping {
        start,
        now: Rc::new(Cell::new(start)),
        sleeps: Vec::new(),
        // The third sleep ends at the deadline; the holder lets go before the launcher resumes.
        step: |at| {
            if at == 200 {
                lock.borrow_mut().take();
            }
        },
    };
    let error = refused_before_surface_with(
        &store,
        &token,
        anyhow::anyhow!("the Herdr launch host would refuse the session directory /state/x"),
        "the Herdr launch host would refuse the session directory",
        start + Duration::from_millis(250),
        &mut clock,
    );
    assert_eq!(clock.sleeps, [100, 100, 50]);
    assert!(
        format!("{error:#}").contains("provider startup is in progress; the claim is retained"),
        "{error:#}"
    );
    let status = store.status().unwrap();
    assert_eq!(
        (status.state, status.error),
        (SessionState::Launching, None)
    );
    assert!(claim_is(&store, &token));
}

// The early decisions create no lock file and change no lock file's permissions before they
// know the directory is this launch's: a replacement directory with another launch's records
// and no lock file gets none, and one whose lock file another account may read keeps it so.
#[test]
fn the_early_decisions_take_only_an_existing_lock_file() {
    let lock_file = |store: &Store| {
        store
            .directory()
            .join(crate::native::session::test_support::TURN_CLAIM_LOCK_FILE)
    };
    let replace = |store: &Store, lock: bool| {
        test_support::apply(
            store,
            &[
                test_support::Change::ReceiptToken,
                test_support::Change::ClaimOther,
            ],
        );
        std::fs::remove_file(lock_file(store)).unwrap();
        if lock {
            std::fs::write(lock_file(store), b"").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(lock_file(store), std::fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
        }
    };
    let unchanged = |store: &Store, lock: bool| {
        assert_eq!(lock_file(store).exists(), lock);
        #[cfg(unix)]
        if lock {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(lock_file(store))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
        let status = store.status().unwrap();
        assert_eq!(
            (status.state, status.error),
            (SessionState::Launching, None)
        );
        assert!(claim_is(store, "successor"));
    };
    for lock in [false, true] {
        // The finished terminal: the replacement as the probe returns, observed at the sleep
        // right after its decision, before the next ordinary poll.
        let (_dir, store, _) = fixture();
        let observed = Cell::new(false);
        let run = run_stepping(
            &store,
            1100,
            |_| {
                replace(&store, lock);
                finished()
            },
            |at| {
                if at == 1000 && !observed.get() {
                    unchanged(&store, lock);
                    observed.set(true);
                }
            },
        );
        assert!(observed.get(), "lock={lock}");
        drop(run);

        // The refusal: the replacement before it settles.
        let (_dir, store, token) = fixture();
        replace(&store, lock);
        let start = Instant::now();
        let mut clock = Simulated {
            now: Rc::new(Cell::new(start)),
            sleeps: Vec::new(),
        };
        let error = refused_before_surface_with(
            &store,
            &token,
            anyhow::anyhow!("the Herdr launch host would refuse the session directory /state/x"),
            "the Herdr launch host would refuse the session directory",
            start + Duration::from_secs(20),
            &mut clock,
        );
        assert_eq!(
            format!("{error:#}"),
            "the Herdr launch host would refuse the session directory /state/x",
            "lock={lock}"
        );
        unchanged(&store, lock);
    }
}

// The production surface observation reaches the terminal's presence check: a surface that
// no adapter of this platform inspects is an inspection error, never absence.
#[cfg(not(windows))]
#[test]
fn the_terminal_surface_presence_reports_an_uninspectable_surface_as_an_error() {
    let (_dir, store, _) = fixture();
    let surface: terminal::TerminalSession = serde_json::from_value(
        serde_json::json!({"terminal": "windows-console", "session_id": "surface"}),
    )
    .unwrap();
    let mut presence = TerminalSurface {
        directory: store.directory(),
        surface: &surface,
    };
    assert!(presence.present(Duration::from_secs(2)).is_err());
}
