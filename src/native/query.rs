//! Read-only views of durable session records. Never recover, send, or close here.
use super::args::{option_value, parse_timeout, set_flag_once, set_once};
use super::envelope;
use super::provider::{self, QuestionEmissions, QuestionObservation};
use super::{DEFAULT_TIMEOUT_SECS, NativeCommand, consent, terminal};
use crate::native::reopen::read_resumed_from;
use crate::native::session::SessionState;
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::schema::{EVENT_READ_LIMIT, SessionEvent};
#[cfg(test)]
use crate::native::session::turn::JournaledEventState;
use crate::native::session::{CoreRecord, RecordReader};
use crate::native::session::{Reader, SessionStatus, launch, requests};
use agent_bridge::{FirstPartyCli, checked_deadline_from, terminal_safe_text};
use anyhow::Context;
use anyhow::{Result, bail};
use serde_json::{Value, json};
#[cfg(test)]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fs, thread};

mod inspect;
pub(super) mod observation;
pub(super) mod request;
pub(super) mod search;
pub(super) mod status;
#[cfg(test)]
mod test_support;
mod timeline;
pub(super) mod wait;
#[cfg(test)]
pub(super) use inspect::inspect_value;
use request::{RequestObservation, elapsed_text, usage_text};
#[cfg(test)]
pub(super) use timeline::timeline_value;

#[derive(Debug)]
pub(super) enum Selector {
    Latest,
    List,
    Event(String),
    Request(String),
}

#[derive(Debug)]
pub(crate) struct ResultRequest {
    id: String,
    selector: Selector,
    wait: bool,
    timeout: Duration,
    json: bool,
}

pub(super) fn parse_inspect(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("inspect requires one session id")?;
    require_valid_session_id(id)?;
    let mut json = false;
    let mut timeline = false;
    let mut request = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--json" => set_flag_once(&mut json, "--json")?,
            "--timeline" => set_flag_once(&mut timeline, "--timeline")?,
            "--request" => {
                let id = option_value(options, &mut index, "--request")?;
                if !requests::valid_id(id) {
                    bail!("invalid Bridge request id")
                }
                set_once(&mut request, id.to_owned(), "--request")?;
            }
            other => bail!("unknown inspect option: {other}"),
        }
        index += 1;
    }
    if request.is_some() && !timeline {
        bail!("--request requires --timeline")
    }
    Ok(NativeCommand::Inspect {
        id: id.clone(),
        json,
        timeline,
        request,
    })
}

pub(super) fn parse_result(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("result requires one session id")?;
    require_valid_session_id(id)?;
    let mut selector = None;
    let mut wait = false;
    let mut timeout = None;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--latest" => set_once(&mut selector, Selector::Latest, "result selector")?,
            "--list" => set_once(&mut selector, Selector::List, "result selector")?,
            "--event" => {
                let id = option_value(options, &mut index, "--event")?;
                if !Reader::valid_event_file_name(id) {
                    bail!("invalid result event id")
                }
                set_once(
                    &mut selector,
                    Selector::Event(id.to_owned()),
                    "result selector",
                )?;
            }
            "--request" => {
                let id = option_value(options, &mut index, "--request")?;
                if !requests::valid_id(id) {
                    bail!("invalid Bridge request id")
                }
                set_once(
                    &mut selector,
                    Selector::Request(id.to_owned()),
                    "result selector",
                )?;
            }
            "--wait" => set_flag_once(&mut wait, "--wait")?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--json" => set_flag_once(&mut json, "--json")?,
            other => bail!("unknown result option: {other}"),
        }
        index += 1;
    }
    let selector = selector.unwrap_or(Selector::Latest);
    if wait && !matches!(selector, Selector::Request(_)) {
        bail!("--wait requires --request; waiting for the latest result could select another turn")
    }
    if timeout.is_some() && !wait {
        bail!("--timeout-secs requires --wait")
    }
    Ok(NativeCommand::Result(ResultRequest {
        id: id.clone(),
        selector,
        wait,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        json,
    }))
}

pub(super) use super::session::SnapshotBusy;
use super::session::snapshot::before_consistency_check;
pub(super) use super::session::snapshot::{PublicationRead, Snapshot};
#[cfg(test)]
pub(super) use super::session::snapshot::{
    with_event_read_count, with_event_read_hook, with_snapshot_hook,
};

impl RequestObservation<'_> {
    fn questions(&self, reader: &Reader) -> Option<QuestionEmissions> {
        self.receipt
            .and_then(|receipt| question_emissions(reader, self.snapshot, receipt))
    }

    fn value_with_questions(&self, reader: &Reader) -> Value {
        let mut value = self.value();
        if let Some(questions) = self.questions(reader) {
            if !questions.items.is_empty() {
                value["question_emissions"] = json!(questions.items);
            }
            question_observation(&mut value, &questions);
        }
        value
    }
}

impl Snapshot {
    pub(super) fn result(&self, reader: &Reader, selector: &Selector) -> Result<Value> {
        Ok(self.observe_result(reader, selector)?.value())
    }

    // Timeline uses cached strict bytes; ordinary queries keep their reader.
    fn result_with(
        &self,
        selector: &Selector,
        read_event: impl FnOnce(&str) -> Result<Option<SessionEvent>>,
    ) -> Result<Value> {
        Ok(self.observe_result_with(selector, read_event)?.value())
    }
}

/// The snapshot every non-search query observes: its publication check compares a
/// journaled event within the fixed [`EVENT_READ_LIMIT`] on every attempt.
pub(super) fn observe_snapshot(reader: &Reader) -> Result<Snapshot> {
    observe_snapshot_with(reader, PublicationRead::Within(EVENT_READ_LIMIT))
}

/// How long a busy snapshot is retried before a read-only query gives up.
const SNAPSHOT_RETRY_WINDOW: Duration = Duration::from_millis(250);

#[cfg(test)]
thread_local! {
    /// Overrides [`SNAPSHOT_RETRY_WINDOW`] on this thread, so a test can force the retry of
    /// a snapshot whose publication read alone outlasts the production window.
    static SNAPSHOT_RETRY_WINDOW_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `run` with busy snapshots retried for `window` instead of
/// [`SNAPSHOT_RETRY_WINDOW`] on this thread.
#[cfg(test)]
pub(super) fn with_snapshot_retry_window<T>(window: Duration, run: impl FnOnce() -> T) -> T {
    SNAPSHOT_RETRY_WINDOW_OVERRIDE.with(|cell| cell.set(Some(window)));
    let outcome = run();
    SNAPSHOT_RETRY_WINDOW_OVERRIDE.with(|cell| cell.set(None));
    outcome
}

fn snapshot_retry_window() -> Duration {
    #[cfg(test)]
    if let Some(window) = SNAPSHOT_RETRY_WINDOW_OVERRIDE.with(std::cell::Cell::get) {
        return window;
    }
    SNAPSHOT_RETRY_WINDOW
}

/// Retries a busy snapshot for [`SNAPSHOT_RETRY_WINDOW`]; every attempt decides a journaled
/// event's publication as `publication` says.
fn observe_snapshot_with(reader: &Reader, publication: PublicationRead) -> Result<Snapshot> {
    observe_snapshot_until(
        reader,
        publication,
        Instant::now() + snapshot_retry_window(),
    )
}

fn observe_snapshot_until(
    reader: &Reader,
    publication: PublicationRead,
    deadline: Instant,
) -> Result<Snapshot> {
    let deadline = deadline.min(Instant::now() + snapshot_retry_window());
    loop {
        match reader.snapshot_with(publication) {
            Err(error) if error.is::<SnapshotBusy>() && Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            outcome => return outcome,
        }
    }
}

// Query observations judge the owner through `observation::judgment`; reopen tests keep this.
#[cfg(test)]
pub(super) use super::session::observe_owner;

/// The public `wait` of one request address, for tests of settlement paths.
#[cfg(test)]
pub(in crate::native) use wait::one as wait_one;

/// Ordinary `result --request`, with question fields, outside wait, failure envelopes and the doctor.
pub(super) fn request_result(reader: &Reader, request_id: &str) -> Result<Value> {
    Ok(observe_snapshot(reader)?
        .observe_result(reader, &Selector::Request(request_id.to_owned()))?
        .value_with_questions(reader))
}

fn question_emissions(
    reader: &Reader,
    snapshot: &Snapshot,
    receipt: &requests::Receipt,
) -> Option<QuestionEmissions> {
    provider::question_emissions(
        reader,
        &snapshot.manifest.provider,
        snapshot._lock.is_some(),
        receipt,
    )
}

fn question_observation(value: &mut Value, questions: &QuestionEmissions) {
    if questions.observation != QuestionObservation::Complete {
        value["question_observation"] = json!(questions.observation);
    }
}

fn question_presence(value: &mut Value, questions: &QuestionEmissions) {
    if !questions.items.is_empty() {
        value["question_emitted"] = json!(true);
    }
    question_observation(value, questions);
}

fn print_result(value: &Value, json: bool) -> Result<()> {
    envelope::emit(json, value, || {
        println!("session: {}", value["session"].as_str().unwrap_or("?"));
        if let Some(id) = value["request_id"].as_str() {
            println!("request: {id}");
        }
        println!(
            "state: {}",
            value["request_state"].as_str().unwrap_or("unknown")
        );
        println!("{}", elapsed_text(value));
        if let Some(usage) = usage_text(value) {
            println!("{usage}");
        }
        for line in question_lines(value) {
            println!("{line}");
        }
        if let Some(message) = value["result"].as_str() {
            println!("\n{}", terminal_safe_text(message, true));
        }
        if let Some(error) = value["error"].as_str() {
            println!("error: {}", terminal_safe_text(error, true));
        }
        Ok(())
    })
}

fn question_lines(value: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for item in value["question_emissions"].as_array().into_iter().flatten() {
        for question in item["questions"].as_array().into_iter().flatten() {
            if let Some(title) = question["title"].as_str() {
                lines.push(format!(
                    "question emitted: {}",
                    terminal_safe_text(title, true)
                ));
            }
            for option in question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                lines.push(format!("  {}", terminal_safe_text(option, true)));
            }
        }
    }
    if let Some(observation) = value["question_observation"].as_str() {
        lines.push(format!(
            "question observation: {}",
            terminal_safe_text(observation, false)
        ));
    }
    lines
}

fn correct_unsuccessful_result(value: &mut Value) {
    value["ok"] = json!(false);
    if value["error"].is_null() {
        value["error"] = json!(match value["request_state"].as_str() {
            Some("cancelled") => "request was cancelled without a successful result",
            Some("unavailable") => "no published result is available for this selection",
            Some("recovery_required") =>
                "completion publication requires recovery; run sessions for this workspace, then query again",
            _ =>
                "request ended without a published successful result; inspect the session before sending another prompt",
        });
    }
}

pub(super) fn run_result(request: ResultRequest) -> Result<()> {
    let outcome = result_value(&request);
    match outcome {
        Ok(mut value) => {
            let empty_selection = matches!(request.selector, Selector::Latest | Selector::Event(_))
                && value["result"].is_null();
            let incomplete_wait = request.wait && value["request_state"] != "completed";
            let unsuccessful = incomplete_wait || empty_selection;
            if unsuccessful {
                correct_unsuccessful_result(&mut value);
            }
            if matches!(request.selector, Selector::List) {
                envelope::emit(request.json, &value, || {
                    for event in value["events"].as_array().into_iter().flatten() {
                        println!(
                            "{}\t{}\t{}\t{}",
                            event["event_id"].as_str().unwrap_or("?"),
                            event["request_state"].as_str().unwrap_or("?"),
                            event["request_id"].as_str().unwrap_or("legacy"),
                            elapsed_text(event)
                        );
                    }
                    Ok(())
                })?;
            } else {
                print_result(&value, request.json)?;
            }
            if unsuccessful {
                bail!(
                    "{}",
                    value["error"]
                        .as_str()
                        .unwrap_or("request did not complete successfully")
                )
            }
            Ok(())
        }
        Err(error) => {
            if request.json {
                envelope::print(&envelope::error(&error, json!({"session": request.id, "request_id": match &request.selector { Selector::Request(id) => Some(id), _ => None },
                "result": null }).as_object().unwrap().clone()))?;
            }
            Err(error)
        }
    }
}

fn result_value(request: &ResultRequest) -> Result<Value> {
    result_value_in(&Reader::state_root()?, request)
}

/// The `result` command's value over one state root, including `--wait`.
pub(super) fn result_value_in(root: &Path, request: &ResultRequest) -> Result<Value> {
    if request.wait {
        let Selector::Request(id) = &request.selector else {
            bail!(
                "--wait requires --request; waiting for the latest result could select another turn"
            )
        };
        return wait::one(root, &request.id, id, request.timeout);
    }
    let directory = Reader::session_directory_in(root, &request.id)?;
    checked_deadline_from(Instant::now(), request.timeout)?;
    let reader = Reader::open_unchecked(&directory);
    if let Selector::Request(id) = &request.selector {
        return request_result(&reader, id);
    }
    let snapshot = observe_snapshot(&reader)?;
    if matches!(request.selector, Selector::List) {
        let mut events = Vec::new();
        for path in &snapshot.paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .context("invalid event file")?;
            let mut value = snapshot.result(
                &Reader::open_unchecked(&directory),
                &Selector::Event(name.to_owned()),
            )?;
            value.as_object_mut().unwrap().remove("result");
            events.push(value);
        }
        let mut requests = Vec::new();
        for receipt in &snapshot.receipts {
            let mut value = snapshot.result(
                &Reader::open_unchecked(&directory),
                &Selector::Request(receipt.request_id.clone()),
            )?;
            value.as_object_mut().unwrap().remove("result");
            requests.push(value);
        }
        return Ok(
            envelope::ok(json!({"session": request.id, "events": events, "requests": requests,
                        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error}).as_object().unwrap().clone()),
        );
    }
    Ok(snapshot
        .observe_result(&reader, &request.selector)?
        .value_with_questions(&reader))
}

pub(super) fn run_inspect(
    id: &str,
    json: bool,
    timeline: bool,
    request: Option<&str>,
) -> Result<()> {
    let outcome = if timeline {
        timeline::run(id, json, request)
    } else {
        inspect::inspect_inner(id, json)
    };
    if let Err(error) = &outcome
        && json
    {
        envelope::print(&envelope::error(
            error,
            json!({"session": id,
            "stored_state": "unknown", })
            .as_object()
            .unwrap()
            .clone(),
        ))?;
    }
    outcome
}

#[cfg(test)]
mod question_tests {
    use super::question_lines;
    use serde_json::json;

    #[test]
    fn human_question_output_escapes_controls_and_preserves_unicode_and_options() {
        let value = json!({"question_emissions":[{"id":"example","questions":[
            {"title":"한국어😀\u{1b}]52;clipboard", "options":["One\u{7}","Two"]}
        ]}],"question_observation":"incomplete"});
        assert_eq!(
            question_lines(&value),
            [
                "question emitted: 한국어😀\\u{1b}]52;clipboard",
                "  One\\u{7}",
                "  Two",
                "question observation: incomplete"
            ]
        );
        assert!(question_lines(&json!({})).is_empty());
        assert_eq!(
            value["question_emissions"][0]["questions"][0]["options"][0],
            "One\u{7}"
        );
    }
}
