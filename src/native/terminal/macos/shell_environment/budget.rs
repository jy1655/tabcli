//! The time budget of the shell environment capture: how long the zsh startup run may take,
//! decided when the capture starts. Pure, so that it compiles and is tested on every target;
//! doctor reads the limit on every platform.
use std::time::{Duration, Instant};

/// The longest a capture runs, whatever time its deadline leaves.
pub(in crate::native) const LIMIT: Duration = Duration::from_secs(10);
/// Time kept after the capture for its cleanup, within the caller's deadline.
pub(in crate::native) const CLEANUP: Duration = Duration::from_millis(500);
/// Time a launch keeps after the capture for its handoff to the launch shell. A doctor
/// probe makes no handoff and adds this back to its deadline.
pub(in crate::native) const HANDOFF_RESERVE: Duration = Duration::from_secs(2);
const RESERVE: Duration = HANDOFF_RESERVE.saturating_add(CLEANUP);

/// The budget of a capture that starts at `start`: what `deadline` leaves after the handoff
/// and cleanup reserves, at most `LIMIT`. Zero leaves no time to start the shell.
pub(in crate::native) fn capture_budget(start: Instant, deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(start)
        .saturating_sub(RESERVE)
        .min(LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_what_the_deadline_leaves_after_both_reserves_at_most_the_limit() {
        assert_eq!(LIMIT, Duration::from_secs(10));
        assert_eq!(CLEANUP, Duration::from_millis(500));
        assert_eq!(HANDOFF_RESERVE, Duration::from_secs(2));
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut actual = Vec::new();
        for (deadline, label) in [
            (start + ms(13_000), "limit"),
            (start + ms(12_500), "exactly the limit"),
            (start + ms(12_499), "the deadline"),
            (start + ms(2_501), "one millisecond"),
            // The handoff and the cleanup are reserved separately: neither alone empties it.
            (start + HANDOFF_RESERVE + CLEANUP, "both reserves"),
            (start + HANDOFF_RESERVE, "the handoff reserve"),
            (start + CLEANUP, "the cleanup reserve"),
            (start, "now"),
        ] {
            actual.push(format!(
                "{label}: {}",
                capture_budget(start, deadline).as_millis()
            ));
        }
        // A deadline already past when the capture starts.
        actual.push(format!(
            "past: {}",
            capture_budget(start + ms(1), start).as_millis()
        ));
        assert_eq!(
            actual,
            [
                "limit: 10000",
                "exactly the limit: 10000",
                "the deadline: 9999",
                "one millisecond: 1",
                "both reserves: 0",
                "the handoff reserve: 0",
                "the cleanup reserve: 0",
                "now: 0",
                "past: 0",
            ]
        );
    }

    // The capture decides its budget when it starts, so time the host spends after its own
    // estimate (its record and the capture thread's start) comes out of the deadline's share,
    // never out of the limit.
    #[test]
    fn a_capture_that_starts_later_keeps_the_limit_and_loses_only_the_deadlines_time() {
        let ms = Duration::from_millis;
        let host = Instant::now();
        let capture = host + ms(40);
        // A deadline 12.52 s away leaves the host the full limit; the capture, 40 ms later,
        // gets what the deadline leaves then.
        let deadline = host + ms(12_520);
        assert_eq!(capture_budget(host, deadline), LIMIT);
        assert_eq!(capture_budget(capture, deadline), ms(9_980));
        // A distant deadline leaves both the limit.
        let deadline = host + ms(60_000);
        assert_eq!(capture_budget(host, deadline), LIMIT);
        assert_eq!(capture_budget(capture, deadline), LIMIT);
        // A deadline the host's estimate just fits is exhausted by the time the capture starts.
        let deadline = host + ms(2_530);
        assert_eq!(capture_budget(host, deadline), ms(30));
        assert_eq!(capture_budget(capture, deadline), Duration::ZERO);
    }
}
