//! The one clock seam of the native module. Production reads the system clock; tests
//! inject a simulated one so that no simulated wait becomes a wall-clock sleep and no
//! simulated deadline depends on the runner's speed.
use std::{
    thread,
    time::{Duration, Instant},
};

/// What a bounded wait, a cooperative budget and a paste gate read time from.
pub(in crate::native) trait Clock {
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

pub(in crate::native) struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

// A simulated clock: `sleep` advances it and adds up what was slept, and `shared` hands
// its instant to a hook or a log reader that moves the clock from outside.
#[cfg(test)]
pub(in crate::native) struct FakeClock {
    now: std::rc::Rc<std::cell::Cell<Instant>>,
    pub(in crate::native) slept: Duration,
}

#[cfg(test)]
impl FakeClock {
    pub(in crate::native) fn new(now: Instant) -> Self {
        Self {
            now: std::rc::Rc::new(std::cell::Cell::new(now)),
            slept: Duration::ZERO,
        }
    }

    pub(in crate::native) fn shared(&self) -> std::rc::Rc<std::cell::Cell<Instant>> {
        std::rc::Rc::clone(&self.now)
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&mut self) -> Instant {
        self.now.get()
    }

    fn sleep(&mut self, duration: Duration) {
        self.now.set(self.now.get() + duration);
        self.slept += duration;
    }
}

// A clock that answers `start` for a given number of reads and `expired` from then on: it
// places the expiry of a budget at one exact read, so a table of cases can expire it at
// each check of a sequence in turn. `reads` counts every read, the expired ones included.
#[cfg(all(test, target_os = "macos"))]
pub(in crate::native) struct ExpiresAtRead {
    start: Instant,
    expired: Instant,
    unexpired_reads: usize,
    reads: std::rc::Rc<std::cell::Cell<usize>>,
}

#[cfg(all(test, target_os = "macos"))]
impl ExpiresAtRead {
    pub(in crate::native) fn new(start: Instant, expired: Instant, unexpired_reads: usize) -> Self {
        Self {
            start,
            expired,
            unexpired_reads,
            reads: std::rc::Rc::default(),
        }
    }

    pub(in crate::native) fn reads(&self) -> std::rc::Rc<std::cell::Cell<usize>> {
        std::rc::Rc::clone(&self.reads)
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Clock for ExpiresAtRead {
    fn now(&mut self) -> Instant {
        let read = self.reads.get();
        self.reads.set(read + 1);
        if read < self.unexpired_reads {
            self.start
        } else {
            self.expired
        }
    }

    fn sleep(&mut self, _: Duration) {
        panic!("a sequence placed on read counts does not sleep")
    }
}
