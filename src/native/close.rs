//! Explicit close of a managed session's visible surface.
use super::args::{parse_timeout, set_flag_once, set_once};
use crate::native::clock::SystemClock;
use crate::native::session::close::budget::CLOSE_TIMEOUT;
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::{Reader, Store};
use crate::native::{NativeCommand, session, terminal};
use agent_bridge::{
    PUBLIC_COMMAND, checked_deadline_from, confirm_explicit_close, terminal_safe_text,
};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
    pub(super) timeout: Duration,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("close-session requires one session id")?;
    require_valid_session_id(id)?;
    let mut explicit = false;
    let mut json = false;
    let mut timeout = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
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
    let started_from = session::origin::scan_started_from(
        directory.parent().unwrap_or(Path::new("")),
        &request.id,
        deadline.min(Instant::now() + session::origin::SCAN_TIME),
        &mut SystemClock,
    );
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
    Ok(())
}

fn started_from_lines(id: &str, started_from: &session::origin::StartedFrom) -> Vec<String> {
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
