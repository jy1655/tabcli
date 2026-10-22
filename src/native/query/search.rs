//! Budgeted substring search over published results.
use super::observe_snapshot_with;
use crate::native::NativeCommand;
use crate::native::args::{WorkspaceScope, option_value, set_flag_once, set_once, workspace_scope};
use crate::native::envelope;
use crate::native::session::Reader;
use crate::native::session::enumerate::{CheckOrder, Fault, Scope, enumerate};
use crate::native::session::schema::SessionEvent;
use crate::native::session::snapshot::PublicationRead;
use crate::native::session::turn::JournaledEventState;
use agent_bridge::{FirstPartyCli, PUBLIC_COMMAND, terminal_safe_text};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

// `search`: substring lookup over published result bodies. Read-only, budgeted, and scoped
// to one workspace unless the caller removes the filter.

const SEARCH_DEFAULT_LIMIT: usize = 20;
const SEARCH_MAX_LIMIT: usize = 200;
const SEARCH_EVENT_BUDGET: usize = 5_000;
const SEARCH_BYTE_BUDGET: u64 = 64 * 1024 * 1024;
pub(super) const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(10);
/// Limit on the displayed excerpt: it is measured after control characters are escaped,
/// so an excerpt never exceeds this many characters however it was sanitised.
const SEARCH_EXCERPT_CHARS: usize = 200;

#[derive(Debug)]
pub(in crate::native) enum SearchScope {
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
pub(in crate::native) fn parse_search(args: &[String]) -> Result<NativeCommand> {
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
pub(super) struct IncompleteReason {
    pub(super) session: Option<String>,
    pub(super) reason: String,
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
pub(in crate::native) fn search_value_in(root: &Path, request: &SearchRequest) -> Result<Value> {
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

pub(in crate::native) fn run_search(request: SearchRequest) -> Result<()> {
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
    use super::{
        SEARCH_BYTE_BUDGET, SEARCH_EXCERPT_CHARS, SearchRequest, check_events_directory, excerpt,
        parse_search, search_error_value,
    };
    use crate::native::NativeCommand;
    use crate::native::session::test_support::EVENTS_DIRECTORY;
    use crate::native::session::{Reader, RecordReader};
    use serde_json::{Value, json};
    use std::fs;

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
