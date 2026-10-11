use super::HOOKS_LOADED_SOURCE;
use super::delivery::{Clock, complete_log_lines, glog_timestamp};
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};

pub(super) const STARTUP_COMPLETED_MARKER: &str = "CLI startup completed";
const SLASH_RELOAD_MARKER: &str = "Reloading system slash commands";
pub(super) const SKILLS_RELOAD_MARKER: &str = "Reloading system slash commands and skills";

const FULL_REDRAW_MARKER: &str = "Full redraw completed";
const REDRAW_AFTER_STARTUP_DESCRIPTION: &str =
    "`Full redraw completed` after `CLI startup completed`";
const DEFERRED_RELOAD_DESCRIPTION: &str = "deferred skills reload or the deferred reload window (no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and outside 1 s after a `Starting new conversation` line, and the window since `CLI startup completed` was observed has not elapsed)";
const QUIET_PERIOD_DESCRIPTION: &str = "quiet period not reached (no `Reloading system slash commands`, `Full redraw completed`, or `hooks_manager.go` line for the quiet period after the newest one)";
const CONVERSATION_START_MARKER: &str = "Starting new conversation";

// A skills reload stamped at most this long after the newest `Starting new
// conversation` line is the reload the new conversation triggers (7 ms and 26 ms
// after it on macOS; 15 ms after the receipt on Windows, session-udT6uY), never the
// trust reload, which follows the Enter that approves the workspace-trust dialog.
const CONVERSATION_RELOAD_MAX_LATENCY: Duration = Duration::from_secs(1);

// Trust reload latency after `CLI startup completed`, Agy 1.2.10 on the Windows
// machine, 2026-09-24 KST (the fixtures in the tests):
//
// | session        | latency | window at the time | paste                       |
// |----------------|---------|--------------------|-----------------------------|
// | session-IEKjtC |  9.8 s  | none (fixed 12 s)  | +9.5 s, lost                |
// | session-fMqSQc | 13.0 s  | none (fixed 12 s)  | +12 s, lost                 |
// | session-ql5TVc | 21.4 s  | 20 s               | +20.1 s, lost               |
// | session-Cf1FBY | 36.4 s  | 35 s               | +35 s, lost (heavy CPU load)|
// | session-uqraap | 36.4 s  | 35 s               | +35 s, lost (CPU idle)      |
// | session-IQHEwf | never   | -                  | none (round-1 gate)         |
//
// Nine more logs put it at 12.9 to 13.2 s, about a second after the fixed 12 s
// paste. The reload always trails the paste, whatever the window was: the paste's
// Enter approved the workspace-trust dialog and Agy logged the trust reload, so each
// longer window only moved the loss later, and session-IQHEwf, which pasted nothing,
// never logged it. No window was ever a measured startup latency, and none is left
// (see the note at the head of this section).
//
// A `Reloading system slash commands and skills` line stamped less than this long
// after `CLI startup completed` is the startup reload, not the trust reload. Agy
// logs its startup reload on either side of `CLI startup completed`: 3.5 ms before
// it in session-fMqSQc, 0.5 ms after it in session-udT6uY and 1.6 ms after it in
// session-M8QFPp (2026-09-24 20:37), where the former rule took it for the later
// reload and settled the condition at once. The trust reload follows an Enter on
// the dialog; the earliest one recorded on the Windows console came 9.8 s after
// startup.
const DEFERRED_RELOAD_MIN_LATENCY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct StartupObservation {
    // Line index of the first `CLI startup completed` line.
    startup_line: Option<usize>,
    // The glog stamp of that line; `None` when it carries none, in which case no
    // skills reload can be proven deferred and only the window settles the condition.
    startup_stamp: Option<Duration>,
    // Line index of the first `Full redraw completed` line after the startup line.
    // Agy redraws the composer once the TUI is up; the redraw before startup, when
    // there is one, does not count.
    pub(super) redraw_after_startup: Option<usize>,
    // Line index of the first `Reloading system slash commands and skills` line
    // after the startup line that is stamped at least `DEFERRED_RELOAD_MIN_LATENCY`
    // after it: the trust reload Agy logs when its workspace-trust dialog is
    // approved (the "deferred" reload of the 2026-09-24 notes). The startup
    // reload, which Agy logs a few milliseconds before or after `CLI startup
    // completed`, does not count on either side of it.
    pub(super) deferred_reload_after_startup: Option<usize>,
    // Line index of the first skills reload after the startup line that was too
    // early to be the deferred reload (diagnostics only).
    pub(super) startup_reload_after_startup: Option<usize>,
    // Line index of the first skills reload after the startup line that followed a
    // `Starting new conversation` line within `CONVERSATION_RELOAD_MAX_LATENCY`: the
    // reload a new conversation triggers, not the deferred one (diagnostics only).
    pub(super) conversation_reload_after_startup: Option<usize>,
    // The glog stamp of the newest `Starting new conversation` line so far.
    conversation_start_stamp: Option<Duration>,
    // Line index of the newest activity line: any `Reloading system slash commands`
    // line (with or without "and skills"), `Full redraw completed` line, or
    // `hooks_manager.go` line, wherever it sits. The quiet period restarts whenever
    // this changes.
    settle_line: Option<usize>,
    // Activity lines seen so far (diagnostics only).
    pub(super) activity_lines: usize,
    // The newest glog stamp in the log: how long Agy itself says it has been running
    // since `CLI startup completed`. A follow-up gated long after startup settles the
    // deferred-reload window from this, not from the gate's own clock, so it does not
    // wait a whole window for a reload that would already have been logged.
    newest_stamp: Option<Duration>,
}

impl StartupObservation {
    pub(super) fn startup_completed(&self) -> bool {
        self.startup_line.is_some()
    }

    // The startup markers the platform rule requires: startup everywhere, plus the
    // redraw where Agy logs one (the Windows console).
    pub(super) fn markers_observed(&self, redraw_required: bool) -> bool {
        self.startup_completed() && (!redraw_required || self.redraw_after_startup.is_some())
    }

    fn missing_markers(&self, redraw_required: bool) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.startup_line.is_none() {
            missing.push("`CLI startup completed`");
        }
        if redraw_required && self.redraw_after_startup.is_none() {
            missing.push(REDRAW_AFTER_STARTUP_DESCRIPTION);
        }
        missing
    }
}

fn is_activity_line(line: &str) -> bool {
    line.contains(SLASH_RELOAD_MARKER)
        || line.contains(FULL_REDRAW_MARKER)
        || line.contains(HOOKS_LOADED_SOURCE)
}

pub(super) fn observe_startup(log: &[u8]) -> StartupObservation {
    let mut observation = StartupObservation::default();
    for (index, line) in complete_log_lines(log).enumerate() {
        if let Some(stamp) = glog_timestamp(&line)
            && observation.newest_stamp.is_none_or(|newest| stamp > newest)
        {
            observation.newest_stamp = Some(stamp);
        }
        if observation.startup_line.is_none() && line.contains(STARTUP_COMPLETED_MARKER) {
            observation.startup_line = Some(index);
            observation.startup_stamp = glog_timestamp(&line);
        }
        if observation.startup_line.is_some()
            && observation.redraw_after_startup.is_none()
            && line.contains(FULL_REDRAW_MARKER)
        {
            observation.redraw_after_startup = Some(index);
        }
        if line.contains(CONVERSATION_START_MARKER)
            && let Some(stamp) = glog_timestamp(&line)
        {
            observation.conversation_start_stamp = Some(stamp);
        }
        if observation
            .startup_line
            .is_some_and(|startup| startup < index)
            && observation.deferred_reload_after_startup.is_none()
            && line.contains(SKILLS_RELOAD_MARKER)
        {
            // Threads stamp out of order by a few milliseconds, so the reload's
            // stamp can precede the startup stamp; a stamp that cannot be read
            // proves nothing and the reload is taken for the startup one.
            let reload_stamp = glog_timestamp(&line);
            let deferred = match (observation.startup_stamp, reload_stamp) {
                (Some(startup), Some(reload)) => {
                    reload.saturating_sub(startup) >= DEFERRED_RELOAD_MIN_LATENCY
                }
                _ => false,
            };
            // A reload right after a conversation start is that conversation's
            // reload: the argument-delivered macOS initial prompt starts one a few
            // seconds after startup, before the deferred reload.
            let conversation_reload = match (observation.conversation_start_stamp, reload_stamp) {
                (Some(started), Some(reload)) => {
                    reload >= started
                        && reload.saturating_sub(started) <= CONVERSATION_RELOAD_MAX_LATENCY
                }
                _ => false,
            };
            if conversation_reload {
                if observation.conversation_reload_after_startup.is_none() {
                    observation.conversation_reload_after_startup = Some(index);
                }
            } else if deferred {
                observation.deferred_reload_after_startup = Some(index);
            } else if observation.startup_reload_after_startup.is_none() {
                observation.startup_reload_after_startup = Some(index);
            }
        }
        if is_activity_line(&line) {
            observation.activity_lines += 1;
            observation.settle_line = Some(index);
        }
    }
    observation
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadinessState {
    Ready,
    AwaitingLog,
    AwaitingStartup,
    AwaitingRedraw,
    AwaitingDeferredReload,
    Settling,
}

impl ReadinessState {
    fn describe(self) -> &'static str {
        match self {
            Self::Ready => "startup readiness observed",
            Self::AwaitingLog => "agy.log has not been created",
            Self::AwaitingStartup => "agy.log has no `CLI startup completed` line",
            Self::AwaitingRedraw => {
                "agy.log has no `Full redraw completed` line after `CLI startup completed`"
            }
            Self::AwaitingDeferredReload => {
                "agy.log has no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and the deferred reload window since `CLI startup completed` has not elapsed"
            }
            Self::Settling => {
                "agy.log logged a reload, redraw, or hooks line within the quiet period"
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ReadinessTiming {
    pub(super) quiet_period: Duration,
    // How long the former rules waited for the trust reload after startup. Zero in
    // every rule of this file; the tests set it to replay those rules.
    pub(super) deferred_reload_window: Duration,
    // Whether a `Full redraw completed` line after startup is required. Agy logs it
    // on the Windows console and never on macOS.
    pub(super) redraw_required: bool,
}

// The settlement instants are only evidence about the log they were measured
// against. Agy appends to its `--log-file`, so every read must extend the previous
// one; a log that disappears, shrinks, or no longer holds every byte observed
// earlier was replaced or rotated, and the indices of its `CLI startup completed`
// and activity lines can coincide with the old ones while the composer behind it
// is fresh. The gate therefore keeps the byte length and a digest of every byte of
// the newest read, the way the receipt check keeps the pre-paste offset, and on a
// discontinuity discards every settlement instant and starts over from the new
// content: the quiet period and the deferred reload window are timed from the new
// observation. Any number of discontinuities is tolerated within the deadline; each
// one restarts the evidence, and the deadline report counts them. A read is a
// continuation only when it is at least as long as the observed content and its
// first `len` bytes digest to the same value, so a rewrite anywhere inside the
// observed content (a re-stamped prefix, a replaced later line, or a suffix rewrite
// that keeps every line index) is a discontinuity, not just one within a leading
// window. The log is small, so the prefix is re-digested on every read.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LogContinuity {
    len: usize,
    digest: u64,
}

fn log_digest(bytes: &[u8]) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

impl LogContinuity {
    fn of(log: &[u8]) -> Self {
        Self {
            len: log.len(),
            digest: log_digest(log),
        }
    }

    // Whether `log` continues the observed content, or how it breaks from it.
    fn discontinuity(&self, log: &[u8]) -> Option<LogDiscontinuity> {
        if log.len() < self.len {
            Some(LogDiscontinuity::Shrunk {
                from: self.len,
                to: log.len(),
            })
        } else if log_digest(&log[..self.len]) != self.digest {
            Some(LogDiscontinuity::Replaced { observed: self.len })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogDiscontinuity {
    // The log disappeared after content had been observed.
    Missing { observed: usize },
    // The log is shorter than the observed content (rotated or truncated).
    Shrunk { from: usize, to: usize },
    // The log no longer begins with every byte observed earlier (replaced).
    Replaced { observed: usize },
}

impl LogDiscontinuity {
    fn describe(self) -> String {
        match self {
            Self::Missing { observed } => {
                format!("agy.log disappeared after {observed} bytes had been observed")
            }
            Self::Shrunk { from, to } => {
                format!("agy.log shrank from {from} to {to} bytes (rotated or truncated)")
            }
            Self::Replaced { observed } => format!(
                "agy.log no longer begins with the bytes observed earlier ({observed} bytes had been observed; replaced)"
            ),
        }
    }
}

struct ReadinessGate {
    timing: ReadinessTiming,
    started_at: Instant,
    settle_line: Option<usize>,
    settled_at: Instant,
    quiet_reached: bool,
    // When the gate first saw `CLI startup completed`; the deferred reload window
    // counts from here, on the gate's clock, like the quiet period.
    startup_seen_at: Option<Instant>,
    deferred_reload_settled: bool,
    last_observation: Option<StartupObservation>,
    // The newest observed log content; `None` until a log has been read and after
    // it disappears.
    continuity: Option<LogContinuity>,
    // Every discontinuity so far, with the time since the gate started.
    discontinuities: Vec<(Duration, LogDiscontinuity)>,
}

impl ReadinessGate {
    fn new(now: Instant, timing: ReadinessTiming) -> Self {
        Self {
            timing,
            started_at: now,
            settle_line: None,
            settled_at: now,
            quiet_reached: false,
            startup_seen_at: None,
            deferred_reload_settled: false,
            last_observation: None,
            continuity: None,
            discontinuities: Vec::new(),
        }
    }

    // Discards every settlement instant: the next observation starts over as if the
    // gate had just been created, with the quiet period and the deferred reload
    // window timed from `now`.
    fn restart_after(&mut self, discontinuity: LogDiscontinuity, now: Instant) {
        self.discontinuities.push((
            now.saturating_duration_since(self.started_at),
            discontinuity,
        ));
        self.settle_line = None;
        self.settled_at = now;
        self.quiet_reached = false;
        self.startup_seen_at = None;
        self.deferred_reload_settled = false;
        self.last_observation = None;
        self.continuity = None;
    }

    // Byte length of the newest observed log; the receipt baseline when that
    // observation passed the gate.
    fn observed_len(&self) -> Option<usize> {
        self.continuity.as_ref().map(|continuity| continuity.len)
    }

    fn observe(&mut self, log: Option<&[u8]>, now: Instant) -> ReadinessState {
        let Some(log) = log else {
            if let Some(previous) = self.continuity.take() {
                self.restart_after(
                    LogDiscontinuity::Missing {
                        observed: previous.len,
                    },
                    now,
                );
            }
            return ReadinessState::AwaitingLog;
        };
        if let Some(discontinuity) = self
            .continuity
            .as_ref()
            .and_then(|previous| previous.discontinuity(log))
        {
            self.restart_after(discontinuity, now);
        }
        self.continuity = Some(LogContinuity::of(log));
        let observation = observe_startup(log);
        if observation.settle_line != self.settle_line {
            self.settle_line = observation.settle_line;
            self.settled_at = now;
        }
        self.quiet_reached =
            now.saturating_duration_since(self.settled_at) >= self.timing.quiet_period;
        if observation.startup_completed() && self.startup_seen_at.is_none() {
            self.startup_seen_at = Some(now);
        }
        // The window is measured on the gate's clock from the first read that showed
        // startup, and also on Agy's own clock from the startup stamp to the newest
        // stamp: a session that has already logged a window's worth of runtime cannot
        // still be inside its startup burst (session-bbNK3d, macOS, 2026-09-24 22:03,
        // never logged the deferred reload and waited the whole window on each of two
        // follow-ups minutes after startup).
        let logged_window_elapsed = match (observation.startup_stamp, observation.newest_stamp) {
            (Some(startup), Some(newest)) => {
                newest.saturating_sub(startup) >= self.timing.deferred_reload_window
            }
            _ => false,
        };
        self.deferred_reload_settled = observation.deferred_reload_after_startup.is_some()
            || logged_window_elapsed
            || self.startup_seen_at.is_some_and(|seen| {
                now.saturating_duration_since(seen) >= self.timing.deferred_reload_window
            });
        let state = if !observation.startup_completed() {
            ReadinessState::AwaitingStartup
        } else if self.timing.redraw_required && observation.redraw_after_startup.is_none() {
            ReadinessState::AwaitingRedraw
        } else if !self.deferred_reload_settled {
            ReadinessState::AwaitingDeferredReload
        } else if !self.quiet_reached {
            ReadinessState::Settling
        } else {
            ReadinessState::Ready
        };
        self.last_observation = Some(observation);
        state
    }

    fn deadline_report(&self, state: ReadinessState) -> String {
        let mut missing = match &self.last_observation {
            Some(observation) => observation.missing_markers(self.timing.redraw_required),
            None => StartupObservation::default().missing_markers(self.timing.redraw_required),
        };
        // No rule of this file has a window; the report names one only for a rule
        // that does.
        let windowed = !self.timing.deferred_reload_window.is_zero();
        if windowed && !self.deferred_reload_settled {
            missing.push(DEFERRED_RELOAD_DESCRIPTION);
        }
        if !self.quiet_reached {
            missing.push(QUIET_PERIOD_DESCRIPTION);
        }
        let missing = if missing.is_empty() {
            "none".to_owned()
        } else {
            missing.join(", ")
        };
        let restarted = match self.discontinuities.len() {
            0 => String::new(),
            1 => format!(
                "; the readiness evidence was restarted after 1 log discontinuity ({})",
                self.describe_discontinuities()
            ),
            count => format!(
                "; the readiness evidence was restarted after {count} log discontinuities ({})",
                self.describe_discontinuities()
            ),
        };
        let timed = if windowed {
            format!(
                "the quiet period is {} ms and the deferred reload window is {} ms, timed concurrently (the window from the first read with `CLI startup completed`, the quiet period from the newest reload, redraw, or hooks line; ready when both hold)",
                self.timing.quiet_period.as_millis(),
                self.timing.deferred_reload_window.as_millis()
            )
        } else {
            format!(
                "the quiet period is {} ms, timed from the newest reload, redraw, or hooks line",
                self.timing.quiet_period.as_millis()
            )
        };
        format!(
            "Agy did not report startup readiness before the deadline: {}; missing markers: {missing}; {timed}{restarted}; the prompt was not pasted",
            state.describe(),
        )
    }

    fn describe_discontinuities(&self) -> String {
        self.discontinuities
            .iter()
            .map(|(at, discontinuity)| {
                format!("{} at {} ms", discontinuity.describe(), at.as_millis())
            })
            .collect::<Vec<_>>()
            .join("; then ")
    }
}

// Waits until one read of the log passes the readiness rule and continues every
// earlier read, and returns that read's byte length: the caller pastes right after
// it, so it is the pre-paste offset for the receipt check. A read that shows a
// discontinuity or new activity is never a baseline; the gate keeps waiting within
// the deadline until a later read passes both.
pub(super) fn wait_for_startup_readiness_with<L, C>(
    read_log: &mut L,
    deadline: Instant,
    timing: ReadinessTiming,
    poll_interval: Duration,
    clock: &mut C,
) -> Result<usize>
where
    L: FnMut() -> Result<Option<Vec<u8>>>,
    C: Clock,
{
    let mut gate = ReadinessGate::new(clock.now(), timing);
    loop {
        let log = read_log().context("Agy startup readiness could not be observed")?;
        let now = clock.now();
        let state = gate.observe(log.as_deref(), now);
        if state == ReadinessState::Ready
            && let Some(observed_len) = gate.observed_len()
        {
            return Ok(observed_len);
        }
        if now >= deadline {
            bail!("{}", gate.deadline_report(state));
        }
        clock.sleep(deadline.saturating_duration_since(now).min(poll_interval));
    }
}

#[cfg(test)]
mod tests;
