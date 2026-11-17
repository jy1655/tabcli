//! Temporary measurement for the Windows CI duration investigation (branch
//! `measure/windows-test-io`; not for merge).
//!
//! Test builds count and time file flushes, directory opens, directory flushes and Windows
//! DACL applications, attributed to the libtest thread that runs each test. A record is
//! written only while `AGENT_BRIDGE_TEST_IO_STATS` names a path prefix; each process
//! appends to `<prefix>.<pid>.jsonl` (tests relaunch this executable), one JSON line when a
//! test thread exits, carrying the pid, executable and Rust temp directory. `AGENT_BRIDGE_TEST_SKIP_FLUSH=1` skips the real flush (the
//! call is still counted, logged by `record_sync` and subject to injected failures).
//! Outside test builds every helper only runs its closure.

#[derive(Clone, Copy)]
pub(in crate::native) enum Op {
    FileFlush,
    DirectoryOpen,
    DirectoryFlush,
    #[cfg_attr(not(windows), allow(dead_code))]
    Dacl,
}

#[cfg(not(test))]
pub(in crate::native) fn timed<T>(_op: Op, run: impl FnOnce() -> T) -> T {
    run()
}

#[cfg(not(test))]
pub(in crate::native) fn flush(
    op: Op,
    run: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let _ = op;
    run()
}

#[cfg(test)]
pub(in crate::native) use measured::{flush, timed};

#[cfg(test)]
mod measured {
    use super::Op;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const OPS: usize = 4;
    const NAMES: [&str; OPS] = ["file_flush", "directory_open", "directory_flush", "dacl"];

    #[derive(Clone, Copy, Default)]
    struct Totals {
        count: [u64; OPS],
        total: [Duration; OPS],
        max: [Duration; OPS],
    }

    impl Totals {
        fn add(&mut self, op: Op, elapsed: Duration) {
            let index = op as usize;
            self.count[index] += 1;
            self.total[index] += elapsed;
            self.max[index] = self.max[index].max(elapsed);
        }

        fn json(&self) -> serde_json::Value {
            let mut map = serde_json::Map::new();
            for (index, name) in NAMES.iter().enumerate() {
                map.insert(
                    (*name).to_owned(),
                    serde_json::json!({
                        "count": self.count[index],
                        "seconds": self.total[index].as_secs_f64(),
                        "max_ms": self.max[index].as_secs_f64() * 1000.0,
                    }),
                );
            }
            serde_json::Value::Object(map)
        }
    }

    struct State {
        output: Option<File>,
        current: Option<String>,
        test: Totals,
        cumulative: Totals,
        unattributed: Totals,
    }

    fn state() -> Option<&'static Mutex<State>> {
        static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();
        STATE
            .get_or_init(|| {
                let prefix = std::env::var_os("AGENT_BRIDGE_TEST_IO_STATS")?;
                let mut path = prefix;
                path.push(format!(".{}.jsonl", std::process::id()));
                let output = OpenOptions::new().create(true).append(true).open(path).ok();
                Some(Mutex::new(State {
                    output,
                    current: None,
                    test: Totals::default(),
                    cumulative: Totals::default(),
                    unattributed: Totals::default(),
                }))
            })
            .as_ref()
    }

    fn skip_flush() -> bool {
        static SKIP: OnceLock<bool> = OnceLock::new();
        *SKIP.get_or_init(|| {
            std::env::var("AGENT_BRIDGE_TEST_SKIP_FLUSH").is_ok_and(|value| value == "1")
        })
    }

    impl State {
        fn write_current(&mut self) {
            let Some(test) = self.current.take() else {
                return;
            };
            let line = serde_json::json!({
                "test": test,
                "ops": self.test.json(),
                "cumulative": self.cumulative.json(),
                "unattributed": self.unattributed.json(),
                "skip_flush": skip_flush(),
                "pid": std::process::id(),
                "executable": std::env::current_exe().ok(),
                "temp_dir": std::env::temp_dir(),
            });
            self.test = Totals::default();
            if let Some(output) = self.output.as_mut() {
                let _ = writeln!(output, "{line}");
            }
        }
    }

    // Dropped when a libtest thread exits, after the test body and its fixtures.
    struct TestThread(String);

    impl Drop for TestThread {
        fn drop(&mut self) {
            if let Some(state) = state() {
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                if state.current.as_deref() == Some(self.0.as_str()) {
                    state.write_current();
                }
            }
        }
    }

    thread_local! {
        static TEST_THREAD: std::cell::RefCell<Option<TestThread>> =
            const { std::cell::RefCell::new(None) };
    }

    fn record(op: Op, elapsed: Duration) {
        let Some(state) = state() else {
            return;
        };
        // libtest names each test thread after the test path; helper threads a test
        // spawns are attributed to the test that is current when they report.
        let name = std::thread::current()
            .name()
            .filter(|name| name.contains("::"))
            .map(str::to_owned);
        if let Some(name) = name.as_ref() {
            let _ = TEST_THREAD.try_with(|guard| {
                let mut guard = guard.borrow_mut();
                if guard.as_ref().map(|thread| &thread.0) != Some(name) {
                    *guard = Some(TestThread(name.clone()));
                }
            });
        }
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(name) = name
            && state.current.as_deref() != Some(name.as_str())
        {
            state.write_current();
            state.current = Some(name);
        }
        state.cumulative.add(op, elapsed);
        if state.current.is_some() {
            state.test.add(op, elapsed);
        } else {
            state.unattributed.add(op, elapsed);
        }
    }

    pub(in crate::native) fn timed<T>(op: Op, run: impl FnOnce() -> T) -> T {
        if state().is_none() {
            return run();
        }
        let started = Instant::now();
        let outcome = run();
        record(op, started.elapsed());
        outcome
    }

    pub(in crate::native) fn flush(
        op: Op,
        run: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        timed(op, || if skip_flush() { Ok(()) } else { run() })
    }
}
