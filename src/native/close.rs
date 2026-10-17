//! Explicit close of a managed session's visible surface.
use super::args::{parse_timeout, set_flag_once, set_once};
use crate::native::clock::{Clock, SystemClock};
use crate::native::session::close::budget::CLOSE_TIMEOUT;
use crate::native::session::origin::{Candidate, StartedFrom, Verified, verify_started_from};
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::{Reader, Store};
use crate::native::{NativeCommand, session, terminal};
use agent_bridge::{
    PUBLIC_COMMAND, checked_deadline_from, confirm_explicit_close, terminal_safe_text,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
    with_started_from: bool,
    pub(super) timeout: Duration,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("close-session requires one session id")?;
    require_valid_session_id(id)?;
    let mut explicit = false;
    let mut json = false;
    let mut with_started_from = false;
    let mut timeout = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            "--with-started-from" => {
                set_flag_once(&mut with_started_from, "--with-started-from")?;
            }
            "--timeout-secs" => {
                index += 1;
                let value = options
                    .get(index)
                    .context("--timeout-secs requires a value")?;
                let duration = parse_timeout(value)?;
                if duration > Duration::from_secs(3600) {
                    bail!("close-session --timeout-secs must be in 1..=3600");
                }
                set_once(&mut timeout, duration, "--timeout-secs")?;
            }
            option => bail!("unknown close-session option: {option}"),
        }
        index += 1;
    }
    confirm_explicit_close(explicit)?;
    Ok(NativeCommand::Close(CloseRequest {
        id: id.to_owned(),
        explicit,
        json,
        with_started_from,
        timeout: timeout.unwrap_or(CLOSE_TIMEOUT),
    }))
}

pub(super) fn run(request: CloseRequest) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    confirm_explicit_close(request.explicit)?;
    let directory = Reader::session_directory(&request.id)?;
    close_session_surface(&directory, &request.id, None, deadline)
        .with_context(|| format!("failed to close visible terminal session {}", request.id))?;
    // Reported after the close succeeded, within what is left of its deadline. The scan
    // reads records only and never fails the close.
    let root = directory.parent().unwrap_or(Path::new(""));
    let started_from = session::origin::scan_started_from(
        root,
        &request.id,
        deadline.min(Instant::now() + session::origin::SCAN_TIME),
        &mut SystemClock,
    );
    if !request.with_started_from {
        if request.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "session": request.id,
                    "closed": true,
                    "started_from": started_from,
                }))?
            );
        } else {
            println!("closed {}", request.id);
            for line in started_from_lines(&request.id, &started_from) {
                println!("{}", terminal_safe_text(&line, false));
            }
        }
        return Ok(());
    }
    // Every listed candidate goes through the same close as an explicit close of it,
    // within the one deadline; the origin link grants nothing.
    let outcome = close_started_from(
        root,
        &request.id,
        &started_from,
        deadline,
        &mut SystemClock,
        &mut |directory, id, reason| close_session_surface(directory, id, Some(reason), deadline),
    );
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome.json(&request.id, &started_from))?
        );
    } else {
        println!("closed {}", request.id);
        for line in outcome.lines(&request.id, &started_from) {
            println!("{}", terminal_safe_text(&line, false));
        }
    }
    // The report is complete above; a failed target still fails the command.
    if !outcome.failed.is_empty() {
        bail!(
            "close-session closed {} but {} of {} started-from session(s) failed to close; see the report",
            request.id,
            outcome.failed.len(),
            started_from.open.len()
        );
    }
    Ok(())
}

fn started_from_lines(id: &str, started_from: &StartedFrom) -> Vec<String> {
    let mut lines = Vec::new();
    if !started_from.open.is_empty() {
        lines.push(format!(
            "{} session(s) started from {id} remain unclosed in the observed records: {}",
            started_from.open.len(),
            started_from.open.join(" ")
        ));
        lines.push(format!(
            "Close a session you no longer need with '{PUBLIC_COMMAND} close-session ID --explicit'."
        ));
    }
    if started_from.incomplete {
        lines.push("Started-from scan incomplete; the list may omit sessions.".to_owned());
    }
    lines
}

/// A closed target with its own report-only started-from scan: those sessions are listed,
/// never closed by this command.
#[derive(Debug, Serialize)]
pub(super) struct ClosedTarget {
    pub(super) session: String,
    pub(super) started_from: StartedFrom,
}

#[derive(Debug, Serialize)]
pub(super) struct SkippedTarget {
    pub(super) session: String,
    pub(super) reason: String,
}

#[derive(Debug, Serialize)]
pub(super) struct FailedTarget {
    pub(super) session: String,
    pub(super) error: String,
}

/// What `--with-started-from` did with each listed candidate, in candidate order.
#[derive(Debug, Default, Serialize)]
pub(super) struct StartedFromClose {
    pub(super) closed: Vec<ClosedTarget>,
    pub(super) skipped: Vec<SkippedTarget>,
    pub(super) failed: Vec<FailedTarget>,
}

const EXHAUSTED: &str = "time_budget_exhausted";

/// The reason a target's closed status keeps when its close creates the tombstone
/// without a repair diagnostic of its own.
pub(super) fn started_from_reason(origin_id: &str) -> String {
    format!("closed by close-session --with-started-from of {origin_id}")
}

/// Closes each candidate of `scan` after re-verifying it, one level deep, within
/// `deadline`. `closer` receives the target's directory, id and reason and is the ordinary
/// explicit close of that session; it is never called for a target the verification
/// rejected, and never twice for one target. A target that fails stays failed; the others
/// are still attempted, so that the report covers every candidate.
pub(super) fn close_started_from(
    root: &Path,
    origin_id: &str,
    scan: &StartedFrom,
    deadline: Instant,
    clock: &mut dyn Clock,
    closer: &mut dyn FnMut(&Path, &str, String) -> Result<()>,
) -> StartedFromClose {
    let mut outcome = StartedFromClose::default();
    for candidate in &scan.candidates {
        close_candidate(
            root,
            origin_id,
            candidate,
            deadline,
            clock,
            closer,
            &mut outcome,
        );
    }
    outcome
}

fn close_candidate(
    root: &Path,
    origin_id: &str,
    candidate: &Candidate,
    deadline: Instant,
    clock: &mut dyn Clock,
    closer: &mut dyn FnMut(&Path, &str, String) -> Result<()>,
    outcome: &mut StartedFromClose,
) {
    let session = candidate.id.clone();
    if clock.now() >= deadline {
        outcome.failed.push(FailedTarget {
            session,
            error: EXHAUSTED.to_owned(),
        });
        return;
    }
    match verify_started_from(root, candidate, origin_id, deadline, clock) {
        Verified::Expired => outcome.failed.push(FailedTarget {
            session,
            error: EXHAUSTED.to_owned(),
        }),
        Verified::Skipped(reason) => outcome.skipped.push(SkippedTarget { session, reason }),
        Verified::Open => {
            match closer(
                &candidate.directory,
                &candidate.id,
                started_from_reason(origin_id),
            ) {
                Ok(()) => {
                    // The target's own started-from sessions are reported, not closed.
                    let now = clock.now();
                    let started_from = session::origin::scan_started_from(
                        root,
                        &candidate.id,
                        deadline.min(now + session::origin::SCAN_TIME),
                        clock,
                    );
                    outcome.closed.push(ClosedTarget {
                        session,
                        started_from,
                    });
                }
                Err(error) => outcome.failed.push(FailedTarget {
                    session,
                    error: format!("{error:#}"),
                }),
            }
        }
    }
}

#[derive(Serialize)]
struct StartedFromReport<'a> {
    #[serde(flatten)]
    scan: &'a StartedFrom,
    closed: &'a [ClosedTarget],
    skipped: &'a [SkippedTarget],
    failed: &'a [FailedTarget],
}

#[derive(Serialize)]
struct CloseReport<'a> {
    ok: bool,
    session: &'a str,
    closed: bool,
    started_from: StartedFromReport<'a>,
}

impl StartedFromClose {
    /// The one JSON object of the flagged close. `closed` says the named session closed;
    /// `ok` is the aggregate and is false when any target failed.
    pub(super) fn json(&self, id: &str, scan: &StartedFrom) -> serde_json::Value {
        serde_json::to_value(CloseReport {
            ok: self.failed.is_empty(),
            session: id,
            closed: true,
            started_from: StartedFromReport {
                scan,
                closed: &self.closed,
                skipped: &self.skipped,
                failed: &self.failed,
            },
        })
        .expect("the close report serializes")
    }

    /// The human lines after `closed ID`: the candidate count, one line per target in
    /// candidate order, and the totals.
    pub(super) fn lines(&self, id: &str, scan: &StartedFrom) -> Vec<String> {
        let mut lines = vec![format!(
            "started-from candidates: {} (scan {})",
            scan.open.len(),
            if scan.incomplete {
                "incomplete"
            } else {
                "complete"
            }
        )];
        let mut closed = self.closed.iter().peekable();
        let mut skipped = self.skipped.iter().peekable();
        let mut failed = self.failed.iter().peekable();
        for session in &scan.open {
            if closed.peek().is_some_and(|t| &t.session == session) {
                let target = closed.next().expect("peeked");
                lines.push(format!(
                    "closed {session} (started from {id}); its own started-from: {} open{}",
                    target.started_from.open.len(),
                    if target.started_from.incomplete {
                        ", incomplete"
                    } else {
                        ""
                    }
                ));
            } else if skipped.peek().is_some_and(|t| &t.session == session) {
                let target = skipped.next().expect("peeked");
                lines.push(format!("skipped {session}: {}", target.reason));
            } else if failed.peek().is_some_and(|t| &t.session == session) {
                let target = failed.next().expect("peeked");
                lines.push(format!("failed {session}: {}", target.error));
            }
        }
        lines.push(format!(
            "started-from close: {} closed, {} skipped, {} failed",
            self.closed.len(),
            self.skipped.len(),
            self.failed.len()
        ));
        lines
    }
}

// Closes a managed session's own visible surface exactly as an explicit `close-session`
// does, through the same terminal-close authority checks. `reason` is kept in the closed
// status when the close itself reports nothing, so a session closed because of a detected
// conflict still says why.
pub(super) fn close_session_surface(
    directory: &Path,
    id: &str,
    reason: Option<String>,
    deadline: std::time::Instant,
) -> Result<()> {
    session::close::close(
        &Store::open_unchecked(directory),
        reason,
        deadline,
        |session| terminal::ownership::close_owned_surface(directory, id, session, deadline),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests;
