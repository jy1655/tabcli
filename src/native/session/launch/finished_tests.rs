//! The launcher's wait when its Herdr terminal finished before startup was confirmed, under
//! the interleavings the settlement must survive: a reader holding the lifecycle lock, a
//! confirmation or launch-host Record published while the probe ran, the path naming another
//! launch, and a launch log that is not an ordinary file. Each runs on a simulated clock with
//! real records and locks.
use super::*;
use std::cell::Cell;
use std::fs;
use std::rc::Rc;

fn root_fixture(root: &std::path::Path, prefix: &str) -> (tempfile::TempDir, Store, String) {
    test_support::pending_launch(prefix, root, START_TIMEOUT)
}

/// A probe that answers that the launch's terminal finished, after `during` ran: what another
/// writer did while the probe was in flight.
struct FinishedProbe<F: FnMut()> {
    during: F,
    probed: Rc<Cell<bool>>,
}

impl<F: FnMut()> SurfacePresence for FinishedProbe<F> {
    fn present(&mut self, _: Duration) -> Result<terminal::LaunchSurface> {
        (self.during)();
        self.probed.set(true);
        Ok(terminal::LaunchSurface::Finished(
            "Herdr terminal t7 finished".into(),
        ))
    }
}

/// A simulated clock. A reader holds the lifecycle lock until the first sleep after the probe.
struct ReaderClock {
    now: Instant,
    reader: Option<fs::File>,
    probed: Rc<Cell<bool>>,
}

impl Clock for ReaderClock {
    fn now(&mut self) -> Instant {
        self.now
    }

    fn sleep(&mut self, duration: Duration) {
        if self.probed.get() {
            self.reader = None;
        }
        self.now += duration;
    }
}

/// Runs the launcher's wait over a probe that reports the terminal finished, with a reader
/// holding the lifecycle lock when `reader` is set.
fn run_finished(store: &Store, reader: bool, during: impl FnMut()) -> Result<()> {
    let probed = Rc::new(Cell::new(false));
    let start = Instant::now();
    let mut clock = ReaderClock {
        now: start,
        reader: reader.then(|| {
            Reader::open_unchecked(store.directory())
                .lock_shared()
                .unwrap()
                .unwrap()
        }),
        probed: Rc::clone(&probed),
    };
    let mut presence = FinishedProbe { during, probed };
    wait_with(store, start + START_TIMEOUT, &mut clock, &mut presence)
}

// `inspect` holds the shared lifecycle lock when the probe finds the terminal finished. The
// observation is kept and the settlement waits for the reader, within the launch deadline,
// instead of returning with the Claim retained.
#[test]
fn a_finished_terminal_settles_after_a_reader_releases_the_lifecycle_lock() {
    let (_dir, store, token) =
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
    let error = run_finished(&store, true, || {}).unwrap_err();
    let text = format!("{error:#}");
    assert_eq!(
        text,
        "terminal surface finished before provider launch was confirmed (Herdr terminal t7 finished)"
    );
    let status = store.status().unwrap();
    assert_eq!(status.state, SessionState::Failed);
    assert_eq!(status.error.as_deref(), Some(text.as_str()));
    assert!(turn::current_claim_token(&store).unwrap().as_deref() != Some(token.as_str()));
}

// A confirmation, or a launch-host Record, published while the probe ran and while a reader
// held the lock keeps its own outcome once the settlement gets the lock.
#[test]
fn what_is_published_during_the_probe_keeps_its_outcome_while_a_reader_holds_the_lock() {
    let (_dir, store, _) =
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
    run_finished(&store, true, || {
        let mut record = read(&store).unwrap().unwrap();
        record.phase = Phase::Spawned;
        store
            .record(CoreRecord::Launch)
            .write_json(&record)
            .unwrap();
    })
    .unwrap();
    assert_eq!(store.status().unwrap().state, SessionState::Launching);

    let core = crate::native::terminal::scripted_herdr_core::ScriptedHerdrCore::start();
    let (_dir, store, token) =
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
    let surface = core.hold_launch(store.directory(), "finished", serde_json::json!({}));
    store.write_terminal(&surface).unwrap();
    // The host publishes under the lock, which the reader holds during the probe: the Record
    // is made first and put in place during the probe, as the host's write lands then.
    host::fail_bound_herdr(
        &store,
        &token,
        &surface,
        host::FailureReason::InputFlushFailed,
        0,
    )
    .unwrap();
    let record = store.record(CoreRecord::LaunchHost);
    let published = record.bytes().unwrap().unwrap();
    record.remove().unwrap();
    let error = run_finished(&store, true, || {
        store
            .record(CoreRecord::LaunchHost)
            .write_private(&published)
            .unwrap();
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("(input_flush_failed)"),
        "{error:#}"
    );
    assert_eq!(store.status().unwrap().error, Some(format!("{error:#}")));
}

// While the probe of launch A ran, A's session directory path came to name launch B's
// directory. A's finished terminal says nothing about B: B keeps its pending launch and its
// Claim, and a confirmed B is not A's confirmation.
#[test]
fn a_finished_report_never_settles_or_confirms_another_launch_at_its_path() {
    for confirmed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (a_dir, a, _) = root_fixture(root.path(), "session-a");
        let (_b_dir, b, b_token) = root_fixture(root.path(), "session-b");
        let path = a.directory().to_owned();
        let other = b.directory().to_owned();
        let moved = root.path().join("moved");
        if confirmed {
            let mut record = read(&b).unwrap().unwrap();
            record.phase = Phase::Spawned;
            b.record(CoreRecord::Launch).write_json(&record).unwrap();
        }
        let b_records = || {
            [
                CoreRecord::Status,
                CoreRecord::TurnClaim,
                CoreRecord::Launch,
            ]
            .map(|record| Store::open_unchecked(&path).record(record).bytes().unwrap())
        };
        let mut before = None;
        let result = run_finished(&a, false, || {
            fs::rename(&path, &moved).unwrap();
            fs::rename(&other, &path).unwrap();
            before = Some(b_records());
        });
        assert!(result.is_err(), "confirmed={confirmed}");
        assert_eq!(Some(b_records()), before, "confirmed={confirmed}");
        let at_path = Store::open_unchecked(&path);
        assert_eq!(
            turn::current_claim_token(&at_path).unwrap().as_deref(),
            Some(b_token.as_str())
        );
        drop(a_dir);
    }
}

// The launch log is read as a diagnostic only: a FIFO there neither blocks the wait nor
// changes its failure.
#[cfg(unix)]
#[test]
fn a_launch_log_that_is_a_fifo_never_blocks_the_finished_failure() {
    let (_dir, store, _) =
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
    let log = store.directory().join(LOG);
    fs::remove_file(&log).unwrap();
    let path = std::ffi::CString::new(log.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let directory = store.directory().to_owned();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let store = Store::open_unchecked(&directory);
        let result = run_finished(&store, false, || {}).map_err(|error| format!("{error:#}"));
        let _ = done.send(result);
    });
    let result = finished
        .recv_timeout(Duration::from_secs(10))
        .expect("the wait blocked on the launch log");
    assert_eq!(
        result.unwrap_err(),
        "terminal surface finished before provider launch was confirmed (Herdr terminal t7 finished)"
    );
}

// The failure renders the launch host's own last Herdr refusal line, bounded and with control
// characters escaped. A line that merely mentions a refusal is not one, and a refusal after
// the first MiB of the log is still found.
#[test]
fn the_finished_failure_renders_the_hosts_last_refusal_line() {
    let cases: [(&str, Vec<u8>, &str); 4] = [
        (
            "plain",
            b"1 launch_pending; waiting for provider spawn\n2 herdr_host_refused: Herdr launch directory is not private to the current user\n".to_vec(),
            "; its launch host logged: Herdr launch directory is not private to the current user",
        ),
        (
            "unrelated",
            b"1 provider printed iterm2_host_refused: something\n2 herdr_host_refused_not_this\n".to_vec(),
            "",
        ),
        (
            "escaped and last",
            b"1 herdr_host_refused: first\n2 herdr_host_refused: red \x1b[31m text\n".to_vec(),
            "; its launch host logged: red \\u{1b}[31m text",
        ),
        (
            "beyond the first MiB",
            [
                vec![b'x'; 1536 * 1024],
                b"\n3 herdr_host_refused: late refusal\n".to_vec(),
            ]
            .concat(),
            "; its launch host logged: late refusal",
        ),
    ];
    for (case, log, suffix) in cases {
        let (_dir, store, _) =
            test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
        fs::write(store.directory().join(LOG), log).unwrap();
        let error = run_finished(&store, false, || {}).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!(
                "terminal surface finished before provider launch was confirmed (Herdr terminal t7 finished){suffix}"
            ),
            "{case}"
        );
    }
    let (_dir, store, _) =
        test_support::pending_launch("session-", &std::env::temp_dir(), START_TIMEOUT);
    fs::write(
        store.directory().join(LOG),
        format!("1 herdr_host_refused: {}\n", "y".repeat(4096)),
    )
    .unwrap();
    let text = format!("{:#}", run_finished(&store, false, || {}).unwrap_err());
    assert!(text.ends_with("…"), "{text}");
    assert!(text.len() < 1024, "{}", text.len());
}
