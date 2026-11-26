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
// answered by the script from its time.
struct Probes<F> {
    start: Instant,
    now: Rc<Cell<Instant>>,
    calls: Vec<(u64, u64)>,
    answer: F,
}

impl<F: FnMut(u64) -> Result<bool>> SurfacePresence for Probes<F> {
    fn present(&mut self, timeout: Duration) -> Result<terminal::LaunchSurface> {
        let at = ms(self.now.get() - self.start);
        self.calls.push((at, ms(timeout)));
        (self.answer)(at).map(terminal::LaunchSurface::from_presence)
    }
}

struct Run {
    result: Result<()>,
    sleeps: Vec<u64>,
    probes: Vec<(u64, u64)>,
}

/// Runs the wait with its deadline `deadline_ms` after its start.
fn run(store: &Store, deadline_ms: u64, answer: impl FnMut(u64) -> Result<bool>) -> Run {
    let start = Instant::now();
    let now = Rc::new(Cell::new(start));
    let mut clock = Simulated {
        now: Rc::clone(&now),
        sleeps: Vec::new(),
    };
    let mut presence = Probes {
        start,
        now,
        calls: Vec::new(),
        answer,
    };
    let token = receipt_token(store);
    let result = wait_with(
        store,
        &ThisLaunch {
            token: &token,
            surface: None,
        },
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
// installed terminal adapters, as the launcher does.
fn run_on_surface(store: &Store, deadline_ms: u64, surface: &terminal::TerminalSession) -> Run {
    let start = Instant::now();
    let mut clock = Simulated {
        now: Rc::new(Cell::new(start)),
        sleeps: Vec::new(),
    };
    let mut presence = TerminalSurface {
        directory: store.directory(),
        surface,
    };
    let token = receipt_token(store);
    let result = wait_with(
        store,
        &ThisLaunch {
            token: &token,
            surface: Some(surface),
        },
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
// surface that disappears does, instead of at the launch deadline. The failure names the
// terminal and its exit code and points to the launch host's own line in the launch log. The
// launch ends as the deadline would have ended it: failed, its Claim released, its receipt
// pending and its surface handle kept for close.
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
        let mut expected = format!(
            "terminal surface finished before provider launch was confirmed (Herdr terminal t7 {})",
            exit_code.map_or("finished".to_owned(), |code| format!(
                "exited with code {code}"
            ))
        );
        if refusal {
            expected.push_str(
                "; its launch host logged: Herdr launch directory is not private to the current user",
            );
        }
        assert_eq!(format!("{error:#}"), expected);
        assert!(error.downcast_ref::<TimedOut>().is_none());
        assert_eq!(run.sleeps, [100; 10], "{exit_code:?}");
        let status = store.status().unwrap();
        assert_eq!(status.state, SessionState::Failed);
        assert_eq!(status.error.as_deref(), Some(expected.as_str()));
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
