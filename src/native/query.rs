//! Read-only views of durable session records. Never recover, send, or close here.
use super::args::{
    WorkspaceScope, option_value, parse_timeout, set_flag_once, set_once, workspace_scope,
};
use super::envelope;
use super::provider::{self, QuestionEmissions, QuestionObservation};
use super::{DEFAULT_TIMEOUT_SECS, NativeCommand, consent, terminal};
use crate::native::reopen::read_resumed_from;
use crate::native::session::SessionState;
use crate::native::session::enumerate::{CheckOrder, Fault, Scope, enumerate};
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::schema::{EVENT_READ_LIMIT, SessionEvent, USAGE_OMITTED_SIZE};
#[cfg(test)]
use crate::native::session::test_support::EVENTS_DIRECTORY;
use crate::native::session::turn::JournaledEventState;
use crate::native::session::{CoreRecord, RecordReader};
use crate::native::session::{Reader, SessionStatus, launch, requests};
use agent_bridge::PUBLIC_COMMAND;
use agent_bridge::{FirstPartyCli, checked_deadline_from, terminal_safe_text};
use anyhow::Context;
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
#[cfg(test)]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fs, thread};

mod inspect;
pub(super) mod observation;
pub(super) mod status;
#[cfg(test)]
mod test_support;
mod timeline;
pub(super) mod wait;
#[cfg(test)]
pub(super) use inspect::inspect_value;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RequestState {
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
    Pending,
    Unresolved,
    Unavailable,
}

impl RequestState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
            Self::Pending => "pending",
            Self::Unresolved => "unresolved",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One Request's meaning, independent of its command's JSON representation. The
/// snapshot retains the lifecycle read lock; each caller keeps its own read policy.
struct RequestObservation<'a> {
    snapshot: &'a Snapshot,
    receipt: Option<&'a requests::Receipt>,
    event_id: Option<&'a str>,
    event: Option<SessionEvent>,
    state: RequestState,
    error: Option<String>,
}

impl RequestObservation<'_> {
    fn value_with_questions(&self, reader: &Reader) -> Value {
        let mut value = self.value();
        if let Some(questions) = self
            .receipt
            .and_then(|receipt| question_emissions(reader, self.snapshot, receipt))
        {
            if !questions.items.is_empty() {
                value["question_emissions"] = json!(questions.items);
            }
            question_observation(&mut value, &questions);
        }
        value
    }

    fn value(&self) -> Value {
        let snapshot = self.snapshot;
        let event = self.event.as_ref();
        let (elapsed, elapsed_reason) = observed_elapsed(self.receipt, event);
        let (usage, usage_reason, usage_error) = reported_usage(event);
        envelope::ok(json!({
            "bridge_observed_elapsed_ms": elapsed, "bridge_observed_elapsed_reason": elapsed_reason,
            "usage": usage, "usage_reason": usage_reason, "usage_error": usage_error,
            "session": snapshot.manifest.id,
            "provider": snapshot.manifest.provider, "workspace": snapshot.manifest.workspace,
            "request_id": self.receipt.map(|r| &r.request_id), "event_id": self.event_id,
            "context_sources": self.receipt.map(|r| r.context_sources.as_slice()).unwrap_or_default(),
            "request_state": self.state, "session_state": snapshot.status.state,
            "result": event.map(|e| &e.message), "error": self.error,
            "session_error": snapshot.status.error,
            "provider_session_id": event.and_then(|e| e.provider_session_id.as_ref()),
            "turn_id": event.and_then(|e| e.turn_id.as_ref()),
            "created_unix_ms": event.map(|e| e.created_unix_ms),
            "recovery_required": snapshot.pending.is_some(),
            "unreadable_requests": snapshot.unreadable_requests,
            "request_index_error": snapshot.request_index_error,
        }).as_object().unwrap().clone())
    }

    fn attachment(
        &self,
        reader: &Reader,
        session: &str,
    ) -> std::result::Result<(requests::ContextSource, String), AttachmentFailure> {
        let refuse = |state, detail| AttachmentFailure { state, detail };
        let unreadable = |detail| refuse("unreadable", Some(detail));
        if self.state != RequestState::Completed || self.event.is_none() || self.error.is_some() {
            return Err(refuse(self.state.as_str(), None));
        }
        let event_id = self
            .event_id
            .ok_or_else(|| unreadable("no event id".to_owned()))?;
        let request_id = self.receipt.map(|r| r.request_id.clone());
        let snapshot = self.snapshot;
        // An unmapped event is legacy only when the entire Receipt index was readable.
        if request_id.is_none()
            && (snapshot.unreadable_requests > 0 || snapshot.request_index_error.is_some())
        {
            let mut detail = format!(
                "the request index has {} unreadable receipt(s), so the event may still belong to an active request",
                snapshot.unreadable_requests
            );
            if let Some(error) = &snapshot.request_index_error {
                detail = format!("{detail}; {error}");
            }
            return Err(refuse("unverifiable", Some(detail)));
        }
        // A case-insensitive filesystem must not silently select a different event name.
        let exact = reader
            .events()
            .map_err(|e| unreadable(format!("{e:#}")))?
            .iter()
            .any(|path| path.file_name().and_then(|name| name.to_str()) == Some(event_id));
        if !exact {
            return Err(unreadable(format!(
                "recorded event filename {event_id} does not match an events/ entry exactly"
            )));
        }
        // Attachments deliberately re-read strictly; ordinary queries and the timeline
        // retain their distinct readers. Do not substitute the lossy event above.
        let event = reader.event_strict(event_id).map_err(unreadable)?;
        if event.cancelled || event.error.is_some() {
            return Err(refuse("failed", None));
        }
        let source = requests::ContextSource {
            session: session.to_owned(),
            request_id,
            event_id: event_id.to_owned(),
            provider: snapshot.manifest.provider.clone(),
            created_unix_ms: event
                .created_unix_ms
                .ok_or_else(|| unreadable("no event creation time".to_owned()))?,
        };
        requests::validate_context_source(&source)
            .map_err(|e| unreadable(format!("invalid recorded provenance: {e:#}")))?;
        Ok((source, event.message))
    }
}

pub(super) struct AttachmentFailure {
    pub(super) state: &'static str,
    pub(super) detail: Option<String>,
}

// Bridge wall-clock time from receipt creation to the published completion event's
// timestamp, including dispatch and delivery waits; never model or billing time.
pub(super) fn observed_elapsed(
    receipt: Option<&requests::Receipt>,
    event: Option<&SessionEvent>,
) -> (Option<u128>, Option<&'static str>) {
    let Some(event) = event else {
        return (None, Some("no_published_result"));
    };
    let Some(receipt) = receipt else {
        return (None, Some("missing_receipt"));
    };
    let Some(start) = receipt.created_unix_ms else {
        return (None, Some("missing_receipt_time"));
    };
    let Some(end) = event.created_unix_ms else {
        return (None, Some("missing_result_time"));
    };
    match end.checked_sub(start) {
        Some(elapsed) => (Some(elapsed), None),
        None => (None, Some("inverted_time")),
    }
}

// The usage the provider itself reported for the request, as the event recorded it, and
// why there is none: `not_reported` (no usage fields on the event: legacy events and
// providers that report nothing), `malformed` (the bounded `usage_error` the writer recorded),
// `omitted_size` (dropped by the event size policy) or `no_published_result`.
pub(super) fn reported_usage(
    event: Option<&SessionEvent>,
) -> (
    Option<&crate::native::session::schema::ProviderUsage>,
    Option<&'static str>,
    Option<&str>,
) {
    let Some(event) = event else {
        return (None, Some("no_published_result"), None);
    };
    if let Some(usage) = &event.usage {
        return (Some(usage), None, None);
    }
    match event.usage_error.as_deref() {
        None => (None, Some("not_reported"), None),
        Some(USAGE_OMITTED_SIZE) => (None, Some("omitted_size"), Some(USAGE_OMITTED_SIZE)),
        Some(error) => (None, Some("malformed"), Some(error)),
    }
}

/// The human line for a result that carries the provider's own usage; none otherwise.
fn usage_text(value: &Value) -> Option<String> {
    let usage = value["usage"].as_object()?;
    let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    let runs = count("runs");
    let provider = value["provider"].as_str().unwrap_or("provider");
    let mut provider_name = provider.chars();
    let provider_name = match provider_name.next() {
        Some(first) => first.to_uppercase().collect::<String>() + provider_name.as_str(),
        None => String::new(),
    };
    Some(format!(
        "usage: {provider_name} reported {} input, {} output, {} cache read, {} cache write, {} total tokens over {runs} run{} (coverage {})",
        count("input_tokens"),
        count("output_tokens"),
        count("cache_read_tokens"),
        count("cache_write_tokens"),
        count("total_tokens"),
        if runs == 1 { "" } else { "s" },
        usage
            .get("coverage")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    ))
}

fn elapsed_text(value: &Value) -> String {
    match &value["bridge_observed_elapsed_ms"] {
        Value::Number(ms) => format!("Bridge observed elapsed: {ms} ms"),
        _ => format!(
            "Bridge observed elapsed: not computable ({})",
            value["bridge_observed_elapsed_reason"]
                .as_str()
                .unwrap_or("unknown")
        ),
    }
}

impl Snapshot {
    pub(super) fn result(&self, reader: &Reader, selector: &Selector) -> Result<Value> {
        Ok(self.observe_result(reader, selector)?.value())
    }

    fn observe_result<'a>(
        &'a self,
        reader: &Reader,
        selector: &'a Selector,
    ) -> Result<RequestObservation<'a>> {
        self.observe_result_with(selector, |name| self.event(reader, name))
    }

    pub(super) fn attachable_result(
        &self,
        reader: &Reader,
        selector: &Selector,
        session: &str,
    ) -> std::result::Result<(requests::ContextSource, String), AttachmentFailure> {
        self.observe_result(reader, selector)
            .map_err(|e| AttachmentFailure {
                state: "unreadable",
                detail: Some(format!("{e:#}")),
            })?
            .attachment(reader, session)
    }

    // Timeline uses cached strict bytes; ordinary queries keep their reader.
    fn result_with(
        &self,
        selector: &Selector,
        read_event: impl FnOnce(&str) -> Result<Option<SessionEvent>>,
    ) -> Result<Value> {
        Ok(self.observe_result_with(selector, read_event)?.value())
    }

    fn observe_result_with<'a>(
        &'a self,
        selector: &'a Selector,
        read_event: impl FnOnce(&str) -> Result<Option<SessionEvent>>,
    ) -> Result<RequestObservation<'a>> {
        let receipt = match selector {
            Selector::Request(id) => Some(
                self.receipts
                    .iter()
                    .find(|receipt| &receipt.request_id == id)
                    .with_context(|| {
                        if self.unreadable_requests > 0 || self.request_index_error.is_some() {
                            format!("no readable receipt for request {id}; the request index is incomplete")
                        } else {
                            format!("no such Bridge request: {id}; legacy events have no request mapping")
                        }
                    })?,
            ),
            _ => None,
        };
        let name = match selector {
            Selector::Request(_) => receipt.map(|r| r.event_file.as_str()),
            Selector::Event(name) => Some(name.as_str()),
            Selector::Latest => self.latest_event_id(),
            Selector::List => bail!("list is not a single result selector"),
        };
        let receipt = receipt.or_else(|| name.and_then(|name| self.receipt_for_event(name)));
        let event = name.map(read_event).transpose()?.flatten();
        if matches!(selector, Selector::Event(_))
            && event.is_none()
            && name.is_some_and(|name| self.published(name))
        {
            bail!("no such recorded event: {}", name.unwrap_or_default())
        }
        let launch_failure = receipt
            .filter(|r| {
                self.launch
                    .as_ref()
                    .is_some_and(|l| l.claim_token == r.claim_token)
            })
            .and_then(|_| {
                launch::diagnostic(self.launch.as_ref(), &self.status, self.claim.as_deref())
            });
        let state = if let Some(event) = &event {
            if event.cancelled {
                RequestState::Cancelled
            } else if event.error.is_some() {
                RequestState::Failed
            } else {
                RequestState::Completed
            }
        } else if name
            .is_some_and(|name| self.pending.as_ref().is_some_and(|p| p.event_file == name))
        {
            RequestState::RecoveryRequired
        } else if launch_failure.is_some() {
            if self.status.state == SessionState::Failed {
                RequestState::Failed
            } else {
                RequestState::Unresolved
            }
        } else if receipt.is_some_and(|r| self.is_active(r)) {
            RequestState::Pending
        } else if receipt.is_some() {
            RequestState::Unresolved
        } else {
            RequestState::Unavailable
        };
        let error = event.as_ref().and_then(|e| e.error.clone()).or_else(|| {
            if event.is_none() {
                launch_failure.map(|(_, detail)| detail)
            } else {
                None
            }
        });
        Ok(RequestObservation {
            snapshot: self,
            receipt,
            event_id: name,
            event,
            state,
            error,
        })
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

// `search`: substring lookup over published result bodies. Read-only, budgeted, and scoped
// to one workspace unless the caller removes the filter.

const SEARCH_DEFAULT_LIMIT: usize = 20;
const SEARCH_MAX_LIMIT: usize = 200;
const SEARCH_EVENT_BUDGET: usize = 5_000;
const SEARCH_BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(10);
/// Limit on the displayed excerpt: it is measured after control characters are escaped,
/// so an excerpt never exceeds this many characters however it was sanitised.
const SEARCH_EXCERPT_CHARS: usize = 200;

#[derive(Debug)]
pub(super) enum SearchScope {
    Workspace(PathBuf),
    AllWorkspaces,
}

impl From<WorkspaceScope> for SearchScope {
    fn from(scope: WorkspaceScope) -> Self {
        match scope {
            WorkspaceScope::Workspace(path) => Self::Workspace(path),
            WorkspaceScope::AllWorkspaces => Self::AllWorkspaces,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SearchRequest {
    query: String,
    scope: SearchScope,
    provider: Option<FirstPartyCli>,
    limit: usize,
    /// Byte budget for event reads. Only the undocumented `--max-bytes` test hook lowers
    /// it below [`SEARCH_BYTE_BUDGET`]; it can never raise it.
    byte_budget: u64,
    json: bool,
}

/// Argument errors keep the documented `--json` contract: when the raw options carry
/// `--json`, the structured failure is printed to stdout before the error propagates.
pub(super) fn parse_search(args: &[String]) -> Result<NativeCommand> {
    match parse_search_options(args) {
        Ok(command) => Ok(command),
        Err(error) => {
            envelope::parse_failure(
                args.get(1..).unwrap_or_default(),
                &error,
                json!({"query": args.first().filter(|query| !query.trim().is_empty()), "hits": []})
                    .as_object()
                    .unwrap()
                    .clone(),
            )?;
            Err(error)
        }
    }
}

/// Environment variable that opens the search test aids (`--max-bytes`) for the
/// integration tests, which drive the release binary and cannot use `cfg(test)`.
const SEARCH_TEST_AIDS_ENV: &str = "AGENT_BRIDGE_TEST_SEARCH_AIDS";

/// Whether the undocumented search test aids are parsed: always in unit tests, and in the
/// binary only when [`SEARCH_TEST_AIDS_ENV`] is set. A production search never sees them.
fn search_test_aids_enabled() -> bool {
    cfg!(test) || std::env::var_os(SEARCH_TEST_AIDS_ENV).is_some_and(|value| !value.is_empty())
}

fn search_error_value(query: Option<&String>, error: &anyhow::Error) -> Value {
    envelope::error(
        error,
        json!({"query": query,
        "hits": []})
        .as_object()
        .unwrap()
        .clone(),
    )
}

fn parse_search_options(args: &[String]) -> Result<NativeCommand> {
    let (query, options) = args.split_first().context("search requires a query")?;
    if query.trim().is_empty() {
        bail!("search requires a non-empty query");
    }
    let mut workspace = None;
    let mut all_workspaces = false;
    let mut provider = None;
    let mut limit = None;
    let mut byte_budget = None;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--all-workspaces" => set_flag_once(&mut all_workspaces, "--all-workspaces")?,
            "--provider" => set_once(
                &mut provider,
                FirstPartyCli::from_str(option_value(options, &mut index, "--provider")?)
                    .map_err(anyhow::Error::msg)?,
                "--provider",
            )?,
            "--limit" => {
                let value = option_value(options, &mut index, "--limit")?;
                let parsed = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid search limit: {value}"))?;
                if !(1..=SEARCH_MAX_LIMIT).contains(&parsed) {
                    bail!("--limit must be between 1 and {SEARCH_MAX_LIMIT}");
                }
                set_once(&mut limit, parsed, "--limit")?;
            }
            // Test aid, deliberately absent from help and the README: lowers the byte
            // budget so an oversized event can be exercised without writing 64 MiB. It is
            // parsed only when the test gate is open (see `search_test_aids_enabled`);
            // otherwise it is an unknown option like any other, and it can never raise the
            // budget above [`SEARCH_BYTE_BUDGET`].
            "--max-bytes" if search_test_aids_enabled() => {
                let value = option_value(options, &mut index, "--max-bytes")?;
                let parsed = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid search byte budget: {value}"))?;
                if !(1..=SEARCH_BYTE_BUDGET).contains(&parsed) {
                    bail!("--max-bytes must be between 1 and {SEARCH_BYTE_BUDGET}");
                }
                set_once(&mut byte_budget, parsed, "--max-bytes")?;
            }
            "--json" => set_flag_once(&mut json, "--json")?,
            other => bail!("unknown search option: {other}"),
        }
        index += 1;
    }
    // Same canonical form that `sessions --workspace` compares against the manifest.
    let scope = SearchScope::from(workspace_scope("search", workspace, all_workspaces)?);
    Ok(NativeCommand::Search(SearchRequest {
        query: query.clone(),
        scope,
        provider,
        limit: limit.unwrap_or(SEARCH_DEFAULT_LIMIT),
        byte_budget: byte_budget.unwrap_or(SEARCH_BYTE_BUDGET),
        json,
    }))
}

#[derive(Serialize)]
struct SearchHit {
    session: String,
    provider: String,
    workspace: PathBuf,
    title: String,
    request_id: Option<String>,
    event_id: String,
    created_unix_ms: Option<u128>,
    excerpt: String,
    result_command: String,
}

#[derive(Serialize)]
struct IncompleteReason {
    session: Option<String>,
    reason: String,
}

struct SearchScan {
    /// Wall-clock deadline fixed before the state root is enumerated, so enumeration,
    /// snapshots, and event reads all draw on the same time budget.
    deadline: Instant,
    byte_budget: u64,
    events_read: usize,
    bytes_read: u64,
    sessions_scanned: usize,
    hits: Vec<SearchHit>,
    reasons: Vec<IncompleteReason>,
}

impl SearchScan {
    fn new(byte_budget: u64) -> Self {
        Self {
            deadline: Instant::now() + SEARCH_TIME_BUDGET,
            byte_budget,
            events_read: 0,
            bytes_read: 0,
            sessions_scanned: 0,
            hits: Vec::new(),
            reasons: Vec::new(),
        }
    }

    /// Names the budget that is already exhausted. Checked before every session,
    /// snapshot, and event read, journaled or not, so a scan never works past its limits.
    fn exhausted_budget(&self) -> Option<String> {
        if self.events_read >= SEARCH_EVENT_BUDGET {
            Some(format!(
                "event budget of {SEARCH_EVENT_BUDGET} reads exhausted"
            ))
        } else if self.bytes_read >= self.byte_budget {
            Some(self.byte_budget_exhausted())
        } else if Instant::now() >= self.deadline {
            Some(format!(
                "time budget of {} s exhausted",
                SEARCH_TIME_BUDGET.as_secs()
            ))
        } else {
            None
        }
    }

    fn byte_budget_exhausted(&self) -> String {
        format!("byte budget of {} bytes exhausted", self.byte_budget)
    }

    fn incomplete(&mut self, session: Option<&str>, reason: impl Into<String>) {
        self.reasons.push(IncompleteReason {
            session: session.map(str::to_owned),
            reason: reason.into(),
        });
    }
}

fn lowercase(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Char index in `message` of the first case-insensitive occurrence of `query_lower`,
/// which must already be folded with [`lowercase`].
fn find_case_insensitive(message: &str, query_lower: &str) -> Option<usize> {
    let lower = lowercase(message);
    let byte_position = lower.find(query_lower)?;
    let target = lower[..byte_position].chars().count();
    let mut folded = 0;
    for (index, character) in message.chars().enumerate() {
        if folded >= target {
            return Some(index);
        }
        folded += character.to_lowercase().count();
    }
    Some(message.chars().count())
}

/// At most [`SEARCH_EXCERPT_CHARS`] displayed characters around the first match. The
/// limit applies after sanitisation: the raw window is cut on a character boundary,
/// sanitised whole, and shrunk again while escape expansion pushes it over the limit,
/// so no escape sequence is ever split and the excerpt never exceeds the limit.
fn excerpt(message: &str, match_start: usize, match_chars: usize) -> String {
    let whole = terminal_safe_text(message, false);
    if whole.chars().count() <= SEARCH_EXCERPT_CHARS {
        return whole;
    }
    let total = message.chars().count();
    // Both cut markers count toward the character limit.
    let mut body = SEARCH_EXCERPT_CHARS - 2;
    loop {
        let start = match_start
            .saturating_sub(body.saturating_sub(match_chars.min(body)) / 2)
            .min(total.saturating_sub(body));
        let end = (start + body).min(total);
        let window = message.chars().skip(start).take(body).collect::<String>();
        let safe = terminal_safe_text(&window, false);
        let markers = usize::from(start > 0) + usize::from(end < total);
        let shown = safe.chars().count() + markers;
        if shown <= SEARCH_EXCERPT_CHARS || body == 0 {
            let mut text = String::new();
            if start > 0 {
                text.push('…');
            }
            text.push_str(&safe);
            if end < total {
                text.push('…');
            }
            return text;
        }
        // Scale the raw window by the observed expansion ratio, always by at least one
        // character, so the loop converges without ever cutting inside an escape.
        let fits = SEARCH_EXCERPT_CHARS - markers;
        body = (body * fits / safe.chars().count()).min(body - 1);
    }
}

/// The shared event listing turns a missing or non-directory `events` path into an
/// empty list; a search must not present that damage as "no results".
fn check_events_directory(reader: &Reader) -> Result<(), String> {
    let directory = reader.directory();
    Reader::open_unchecked(directory)
        .require_events_directory()
        .map_err(|error| format!("{error:#}"))
}

enum SessionScan {
    Done,
    Budget(String),
}

fn search_session(
    request: &SearchRequest,
    query_lower: &str,
    reader: &Reader,
    id: &str,
    scan: &mut SearchScan,
) -> Result<SessionScan, String> {
    // The manifest alone decides scope, so out-of-scope sessions are never snapshotted.
    let manifest = reader.manifest().map_err(|error| format!("{error:#}"))?;
    if !(Scope {
        workspace: match &request.scope {
            SearchScope::Workspace(path) => Some(path),
            SearchScope::AllWorkspaces => None,
        },
        provider: request.provider,
    })
    .admits(&manifest)
    {
        return Ok(SessionScan::Done);
    }
    if let Some(budget) = scan.exhausted_budget() {
        return Ok(SessionScan::Budget(budget));
    }
    // `events` is validated before anything is read through it, and again after the
    // snapshot listed it.
    check_events_directory(reader)?;
    // The snapshot reads no event: a journaled event's publication is decided below, at
    // the event's own position in the scan, so a busy retry costs nothing and the
    // journal's presence never changes which records the scan examines before it.
    let snapshot = observe_snapshot_with(reader, PublicationRead::Deferred)
        .map_err(|error| format!("{error:#}"))?;
    scan.sessions_scanned += 1;
    check_events_directory(reader)?;
    // A damaged request index loses event-to-request mappings. Events that still have a
    // readable receipt are searched; the rest are skipped and counted, never reported
    // as legacy `request_id: null` hits.
    let index_incomplete =
        snapshot.unreadable_requests > 0 || snapshot.request_index_error.is_some();
    let mut without_receipt = 0usize;
    for path in &snapshot.paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let journal = snapshot.journal_for(name);
        if journal.is_none() && !snapshot.published(name) {
            continue;
        }
        let receipt = snapshot.receipt_for_event(name);
        if index_incomplete && receipt.is_none() {
            without_receipt += 1;
            continue;
        }
        // Every budget, the event count included, is checked before the record is
        // physically read, whether the read is an ordinary one or the publication
        // comparison of the journaled event.
        if let Some(budget) = scan.exhausted_budget() {
            return Ok(SessionScan::Budget(budget));
        }
        let remaining = scan.byte_budget.saturating_sub(scan.bytes_read);
        let read = match journal {
            Some(_) => {
                // The publication comparison is the journaled event's one read: it is
                // charged like any other, and a committed event is searched from the
                // bytes it compared. A record the budget cannot hold stops the scan with
                // the same reason an ordinary record would, so the result is the same
                // once the journal is gone and the event is read the ordinary way.
                let read = snapshot
                    .publication_read(reader, name, remaining)
                    .map_err(|error| format!("{name}: {error:#}"))?;
                scan.bytes_read += read.bytes_read;
                match read.state {
                    JournaledEventState::Committed => Ok(read.committed_text),
                    JournaledEventState::Absent => Ok(None),
                    JournaledEventState::Oversized(size) => Err(Ok(size)),
                    JournaledEventState::Mismatched => {
                        scan.events_read += 1;
                        scan.incomplete(
                            Some(id),
                            format!(
                                "{name}: skipped; the event differs from its pending completion journal and is not published"
                            ),
                        );
                        continue;
                    }
                }
            }
            None => reader
                .event(name)
                .event_within_budget(remaining)
                .inspect(|text| {
                    if let Some(text) = text {
                        scan.bytes_read += text.len() as u64;
                    }
                }),
        };
        let event: SessionEvent = match read {
            Ok(Some(text)) => {
                scan.events_read += 1;
                match serde_json::from_str(&text) {
                    Ok(event) => event,
                    Err(error) => {
                        scan.incomplete(Some(id), format!("{name}: invalid JSON: {error}"));
                        continue;
                    }
                }
            }
            Ok(None) => {
                scan.incomplete(Some(id), format!("{name}: disappeared during the scan"));
                continue;
            }
            Err(Ok(size)) => {
                return Ok(SessionScan::Budget(format!(
                    "{}; {id}/{name} is {size} bytes with {remaining} bytes remaining",
                    scan.byte_budget_exhausted()
                )));
            }
            Err(Err(error)) => {
                scan.incomplete(Some(id), format!("{name}: {error:#}"));
                continue;
            }
        };
        if event.cancelled || event.error.is_some() {
            continue;
        }
        let Some(start) = find_case_insensitive(&event.message, query_lower) else {
            continue;
        };
        let request_id = receipt.map(|receipt| receipt.request_id.clone());
        let result_command = match &request_id {
            Some(request_id) => {
                format!("{PUBLIC_COMMAND} result {id} --request {request_id} --json")
            }
            None => format!("{PUBLIC_COMMAND} result {id} --event {name} --json"),
        };
        scan.hits.push(SearchHit {
            session: id.to_owned(),
            provider: manifest.provider.clone(),
            workspace: manifest.workspace.clone(),
            title: manifest.title.clone(),
            request_id,
            event_id: name.to_owned(),
            created_unix_ms: event.created_unix_ms,
            excerpt: excerpt(&event.message, start, request.query.chars().count()),
            result_command,
        });
    }
    if index_incomplete {
        let detail = match &snapshot.request_index_error {
            Some(error) => error.clone(),
            None => format!("{} unreadable receipt(s)", snapshot.unreadable_requests),
        };
        scan.incomplete(
            Some(id),
            format!(
                "request index incomplete: {detail}; {without_receipt} event(s) without a readable receipt skipped"
            ),
        );
    }
    Ok(SessionScan::Done)
}

fn search_value(request: &SearchRequest) -> Result<Value> {
    search_value_in(&Reader::state_root()?, request)
}

/// The `search` command's value over one state root.
pub(super) fn search_value_in(root: &Path, request: &SearchRequest) -> Result<Value> {
    // The clock starts before enumeration so slow roots count against the budget too.
    let mut scan = SearchScan::new(request.byte_budget);
    let mut ids = Vec::new();
    match enumerate(root, CheckOrder::IdFirst) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => ids.push(entry.id),
                    Err(Fault::Entry(error)) => {
                        return Err(error).context("failed to read the native state root");
                    }
                    Err(Fault::FileType { id, error }) => scan.incomplete(
                        None,
                        format!("session entry {id}: failed to read its file type: {error}"),
                    ),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read the native state root {}", root.display())
            });
        }
    }
    // Deterministic order keeps budget cut-offs and reasons reproducible.
    ids.sort();
    let query_lower = lowercase(&request.query);
    for id in &ids {
        let budget = match scan.exhausted_budget() {
            Some(budget) => Some(budget),
            None => match search_session(
                request,
                &query_lower,
                &Reader::open_unchecked(root.join(id)),
                id,
                &mut scan,
            ) {
                Ok(SessionScan::Done) => None,
                Ok(SessionScan::Budget(budget)) => Some(budget),
                Err(reason) => {
                    scan.incomplete(Some(id), reason);
                    None
                }
            },
        };
        if let Some(budget) = budget {
            scan.incomplete(None, format!("scan stopped: {budget}"));
            break;
        }
    }
    scan.hits.sort_by(|left, right| {
        right
            .created_unix_ms
            .cmp(&left.created_unix_ms)
            .then_with(|| left.session.cmp(&right.session))
            .then_with(|| left.event_id.cmp(&right.event_id))
    });
    let truncated = scan.hits.len() > request.limit;
    scan.hits.truncate(request.limit);
    let (workspace, all_workspaces) = match &request.scope {
        SearchScope::Workspace(path) => (Some(path), false),
        SearchScope::AllWorkspaces => (None, true),
    };
    Ok(envelope::ok(
        json!({
            "query": request.query,
            "filters": {
                "workspace": workspace, "all_workspaces": all_workspaces,
                "provider": request.provider.map(FirstPartyCli::as_str),
            },
            "limit": request.limit, "hits": scan.hits, "truncated": truncated,
            "incomplete": !scan.reasons.is_empty(), "incomplete_reasons": scan.reasons,
            "scanned": { "sessions": scan.sessions_scanned, "events": scan.events_read },
        })
        .as_object()
        .unwrap()
        .clone(),
    ))
}

pub(super) fn run_search(request: SearchRequest) -> Result<()> {
    let value = match search_value(&request) {
        Ok(value) => value,
        Err(error) => {
            if request.json {
                envelope::print(&search_error_value(Some(&request.query), &error))?;
            }
            return Err(error);
        }
    };
    envelope::emit(request.json, &value, || {
        let hits = value["hits"].as_array().cloned().unwrap_or_default();
        for hit in &hits {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                hit["session"].as_str().unwrap_or("?"),
                terminal_safe_text(hit["provider"].as_str().unwrap_or("?"), false),
                hit["created_unix_ms"],
                hit["request_id"]
                    .as_str()
                    .or(hit["event_id"].as_str())
                    .unwrap_or("?"),
                hit["excerpt"].as_str().unwrap_or(""),
            );
        }
        let reasons = value["incomplete_reasons"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let scanned = format!(
            "{} session(s), {} event(s) examined",
            value["scanned"]["sessions"], value["scanned"]["events"]
        );
        if hits.is_empty() && reasons.is_empty() {
            println!("no results ({scanned})");
        } else if hits.is_empty() {
            println!("no hits, but the scan was incomplete ({scanned})");
        } else {
            let truncated = if value["truncated"].as_bool().unwrap_or(false) {
                format!("; truncated to --limit {}", value["limit"])
            } else {
                String::new()
            };
            println!("{} hit(s) ({scanned}{truncated})", hits.len());
        }
        if !reasons.is_empty() {
            println!(
                "incomplete: {} reason(s); not every stored result was examined",
                reasons.len()
            );
            for reason in &reasons {
                println!(
                    "  {}: {}",
                    reason["session"].as_str().unwrap_or("scan"),
                    terminal_safe_text(reason["reason"].as_str().unwrap_or("?"), false)
                );
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn excerpt_limit_applies_to_the_sanitised_text_without_splitting_escapes() {
        // Every raw character expands to six displayed characters, so a raw cut at
        // 198 characters would display far more than the limit.
        let message = "\x1b".repeat(400);
        let text = excerpt(&message, 200, 1);
        assert!(text.chars().count() <= SEARCH_EXCERPT_CHARS, "{text}");
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
        let body = text.trim_matches('…');
        assert!(!body.is_empty());
        // The displayed body is whole `\u{1b}` escapes only: nothing was split.
        let escaped = r"\u{1b}";
        assert_eq!(
            body.matches(escaped).count() * escaped.len(),
            body.len(),
            "{body}"
        );
        assert_eq!(text.chars().count(), SEARCH_EXCERPT_CHARS, "{text}");

        // A short message whose escapes push it over the limit is cut, not returned whole.
        let short = format!("{}needle", "\x07".repeat(60));
        let text = excerpt(&short, 60, 6);
        assert!(text.chars().count() <= SEARCH_EXCERPT_CHARS, "{text}");
        assert!(text.contains("needle"), "{text}");

        // Plain text keeps the previous behaviour: window plus both markers is exactly 200.
        let plain = "a".repeat(1000);
        let text = excerpt(&plain, 500, 1);
        assert_eq!(text.chars().count(), SEARCH_EXCERPT_CHARS);
        assert!(text.starts_with('…') && text.ends_with('…'));
        assert_eq!(excerpt("short", 0, 5), "short");
    }

    #[test]
    fn search_argument_errors_are_structured_only_when_json_was_requested() {
        let value = search_error_value(Some(&"needle".to_owned()), &anyhow::anyhow!("boom"));
        assert_eq!(
            value,
            json!({"schema_version": 1, "ok": false, "query": "needle", "error": "boom", "hits": []})
        );
        let value = search_error_value(None, &anyhow::anyhow!("boom"));
        assert_eq!(value["query"], Value::Null);
        let args = ["needle", "--max-bytes", "0", "--json"].map(str::to_owned);
        let error = parse_search(&args).unwrap_err().to_string();
        assert!(
            error.contains("--max-bytes must be between 1 and"),
            "{error}"
        );
        let args = ["needle", "--max-bytes", "1"].map(str::to_owned);
        assert!(matches!(
            parse_search(&args).unwrap(),
            NativeCommand::Search(SearchRequest { byte_budget: 1, .. })
        ));
        let args = [
            "needle",
            "--max-bytes",
            &(SEARCH_BYTE_BUDGET + 1).to_string(),
        ]
        .map(str::to_owned);
        assert!(parse_search(&args).is_err());
    }

    #[test]
    fn events_are_never_read_past_the_remaining_byte_budget() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("event.json");
        fs::write(&path, b"0123456789").unwrap();
        assert_eq!(
            RecordReader::at(&path)
                .event_within_budget(10)
                .unwrap()
                .unwrap(),
            "0123456789"
        );
        assert_eq!(
            RecordReader::at(&path)
                .event_within_budget(9)
                .unwrap_err()
                .unwrap(),
            10
        );
        assert!(
            RecordReader::at(&directory.path().join("missing.json"))
                .event_within_budget(10)
                .unwrap()
                .is_none()
        );
        let error = RecordReader::at(directory.path())
            .event_within_budget(10)
            .unwrap_err()
            .unwrap_err();
        assert!(error.to_string().contains("non-regular"), "{error}");
        assert!(check_events_directory(&Reader::open_unchecked(directory.path())).is_err());
        fs::write(directory.path().join(EVENTS_DIRECTORY), "x").unwrap();
        assert_eq!(
            check_events_directory(&Reader::open_unchecked(directory.path())).unwrap_err(),
            "events is not a directory"
        );
    }
}

#[cfg(test)]
mod elapsed_tests {
    use super::*;

    #[test]
    fn human_usage_line_names_counts_runs_and_coverage() {
        let usage = json!({"provider": "pi", "usage": {"source": "pi_extension_agent_end",
            "scope": "request", "coverage": "complete", "runs": 1, "assistant_messages": 1,
            "input_tokens": 1234, "output_tokens": 567, "cache_read_tokens": 0,
            "cache_write_tokens": 0, "total_tokens": 1801}});
        assert_eq!(
            usage_text(&usage).unwrap(),
            "usage: Pi reported 1234 input, 567 output, 0 cache read, 0 cache write, 1801 total tokens over 1 run (coverage complete)"
        );
        let mut uncertain = usage.clone();
        uncertain["usage"]["coverage"] = json!("uncertain");
        uncertain["usage"]["runs"] = json!(2);
        assert!(
            usage_text(&uncertain)
                .unwrap()
                .ends_with("over 2 runs (coverage uncertain)")
        );
        assert_eq!(
            usage_text(&json!({"usage": null, "usage_reason": "not_reported"})),
            None
        );
    }

    #[test]
    fn reported_usage_reasons_follow_the_recorded_fields() {
        let mut event = SessionEvent {
            provenance: None,
            cancelled: false,
            provider: "pi".to_owned(),
            message: "m".to_owned(),
            error: None,
            provider_session_id: None,
            turn_id: None,
            created_unix_ms: Some(1),
            usage: None,
            usage_error: None,
        };
        assert_eq!(
            reported_usage(None),
            (None, Some("no_published_result"), None)
        );
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("not_reported"), None)
        );
        event.usage_error = Some(USAGE_OMITTED_SIZE.to_owned());
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("omitted_size"), Some(USAGE_OMITTED_SIZE))
        );
        event.usage_error = Some("malformed usage: runs".to_owned());
        assert_eq!(
            reported_usage(Some(&event)),
            (None, Some("malformed"), Some("malformed usage: runs"))
        );
    }

    #[test]
    fn human_elapsed_distinguishes_zero_from_uncomputable() {
        assert_eq!(
            elapsed_text(&json!({"bridge_observed_elapsed_ms": 0})),
            "Bridge observed elapsed: 0 ms"
        );
        assert_eq!(
            elapsed_text(
                &json!({"bridge_observed_elapsed_ms": null, "bridge_observed_elapsed_reason": "missing_receipt"})
            ),
            "Bridge observed elapsed: not computable (missing_receipt)"
        );
    }

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
