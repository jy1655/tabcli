//! Read-only views of durable session records. Never recover, send, or close here.
use super::*;
use serde_json::{Value, json};

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
    let json = match options {
        [] => false,
        [option] if option == "--json" => true,
        _ => bail!("inspect accepts only --json"),
    };
    Ok(NativeCommand::Inspect {
        id: id.clone(),
        json,
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
                if !valid_event_file_name(id) {
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

#[derive(Debug)]
pub(super) struct SnapshotBusy;
impl std::fmt::Display for SnapshotBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session records are changing; retry the read-only query")
    }
}
impl std::error::Error for SnapshotBusy {}

pub(super) struct Snapshot {
    pub(super) manifest: SessionManifest,
    pub(super) status: SessionStatus,
    pub(super) receipts: Vec<requests::Receipt>,
    pub(super) unreadable_requests: usize,
    pub(super) request_index_error: Option<String>,
    paths: Vec<PathBuf>,
    pub(super) claim: Option<String>,
    pub(super) pending: Option<PendingTurnCompletion>,
    _lock: Option<File>,
}

pub(super) fn optional_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    read_regular_text_if_present(path)?
        .map(|text| {
            serde_json::from_str(&text)
                .with_context(|| format!("invalid JSON in {}", path.display()))
        })
        .transpose()
}

impl Snapshot {
    pub(super) fn read(directory: &Path) -> Result<Self> {
        // Open an existing lifecycle lock without creating it or changing permissions.
        let lock_path = directory.join(TURN_CLAIM_LOCK_FILE);
        let lock = match File::open(&lock_path) {
            Ok(file) => {
                match file.try_lock_shared() {
                    Ok(()) => (),
                    Err(std::fs::TryLockError::WouldBlock) => return Err(SnapshotBusy.into()),
                    Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                }
                Some(file)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("failed to observe native lifecycle lock"),
        };
        let state_files = ["status.json", TURN_CLAIM_FILE, TURN_COMPLETION_FILE];
        let before = state_files
            .map(|name| read_regular_text_if_present(&directory.join(name)))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let status: SessionStatus = serde_json::from_str(
            before[0]
                .as_deref()
                .context("session has no status record")?,
        )?;
        let pending: Option<PendingTurnCompletion> =
            before[2].as_deref().map(serde_json::from_str).transpose()?;
        if let Some(pending) = &pending {
            validate_pending_completion(pending)?;
        }
        let (index, request_index_error) = match requests::list(directory) {
            Ok(index) => (index, None),
            Err(error) => (requests::Index::default(), Some(format!("{error:#}"))),
        };
        let snapshot = Self {
            manifest: read_manifest(directory)?,
            status,
            receipts: index.receipts,
            unreadable_requests: index.unreadable,
            request_index_error,
            paths: event_paths(directory)?,
            claim: before[1].as_deref().map(|text| text.trim().to_owned()),
            pending,
            _lock: lock,
        };
        // Status also has its own writer lock. Check for a moving snapshot even with a
        // lifecycle reader lock, and support old records that have no lock file at all.
        for (index, name) in state_files.iter().enumerate() {
            if read_regular_text_if_present(&directory.join(name))? != before[index] {
                return Err(SnapshotBusy.into());
            }
        }
        Ok(snapshot)
    }

    fn receipt_for_event(&self, event: &str) -> Option<&requests::Receipt> {
        self.receipts
            .iter()
            .find(|receipt| receipt.event_file == event)
    }

    fn published(&self, event: &str) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.event_file == event)
        {
            return false;
        }
        !self
            .receipt_for_event(event)
            .is_some_and(|receipt| self.claim.as_deref() == Some(&receipt.claim_token))
    }

    fn event(&self, directory: &Path, name: &str) -> Result<Option<SessionEvent>> {
        if !self.published(name) {
            return Ok(None);
        }
        optional_json(&directory.join("events").join(name))
    }

    pub(super) fn result(&self, directory: &Path, selector: &Selector) -> Result<Value> {
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
            Selector::Latest => self
                .paths
                .iter()
                .rev()
                .filter_map(|p| p.file_name()?.to_str())
                .find(|name| self.published(name)),
            Selector::List => bail!("list is not a single result selector"),
        };
        let receipt = receipt.or_else(|| name.and_then(|name| self.receipt_for_event(name)));
        let event = name
            .map(|name| self.event(directory, name))
            .transpose()?
            .flatten();
        if matches!(selector, Selector::Event(_))
            && event.is_none()
            && name.is_some_and(|name| self.published(name))
        {
            bail!("no such recorded event: {}", name.unwrap_or_default())
        }
        let state = if let Some(event) = &event {
            if event.error.is_some() {
                "failed"
            } else {
                "completed"
            }
        } else if name
            .is_some_and(|name| self.pending.as_ref().is_some_and(|p| p.event_file == name))
        {
            "recovery_required"
        } else if receipt.is_some_and(|r| self.claim.as_deref() == Some(&r.claim_token)) {
            "pending"
        } else if receipt.is_some() {
            "unresolved"
        } else {
            "unavailable"
        };
        Ok(json!({
            "schema_version": 1, "ok": true, "session": self.manifest.id,
            "provider": self.manifest.provider, "workspace": self.manifest.workspace,
            "request_id": receipt.map(|r| &r.request_id), "event_id": name,
            "context_sources": receipt.map(|r| r.context_sources.as_slice()).unwrap_or_default(),
            "request_state": state, "session_state": self.status.state,
            "result": event.as_ref().map(|e| &e.message),
            "error": event.as_ref().and_then(|e| e.error.as_ref()),
            "session_error": self.status.error,
            "provider_session_id": event.as_ref().and_then(|e| e.provider_session_id.as_ref()),
            "turn_id": event.as_ref().and_then(|e| e.turn_id.as_ref()),
            "created_unix_ms": event.as_ref().map(|e| e.created_unix_ms),
            "recovery_required": self.pending.is_some(),
            "unreadable_requests": self.unreadable_requests, "request_index_error": self.request_index_error,
        }))
    }
}

pub(super) fn observe_snapshot(directory: &Path) -> Result<Snapshot> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match Snapshot::read(directory) {
            Err(error) if error.is::<SnapshotBusy>() && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            outcome => return outcome,
        }
    }
}

#[derive(Default, Serialize)]
pub(super) struct OwnerObservation {
    pub(super) process_alive: Option<bool>,
    pub(super) identity_matches: Option<bool>,
    pub(super) error: Option<String>,
}

pub(super) fn observe_owner(directory: &Path) -> OwnerObservation {
    let owner = match optional_json::<NativeSessionOwner>(&directory.join(SESSION_OWNER_FILE)) {
        Ok(Some(owner)) => owner,
        Ok(None) => return OwnerObservation::default(),
        Err(error) => {
            return OwnerObservation {
                error: Some(format!("{error:#}")),
                ..OwnerObservation::default()
            };
        }
    };
    observe_owner_record(&owner)
}

pub(super) fn observe_owner_record(owner: &NativeSessionOwner) -> OwnerObservation {
    let observation = OwnerObservation {
        process_alive: Some(process_is_alive(owner.pid)),
        ..OwnerObservation::default()
    };
    if observation.process_alive == Some(false) {
        return observation;
    }
    #[cfg(target_os = "macos")]
    let identity = (owner.process_start_seconds.is_some()
        && owner.process_start_microseconds.is_some()
        && owner.terminal_tty_device.is_some()
        && owner.process_group.is_some()
        && owner.terminal_process_group.is_some())
    .then(|| mac_native_owner_is_live(owner));
    #[cfg(windows)]
    let identity = owner.windows_process_identity.as_ref().map(|identity| {
        terminal::verify_windows_process_identity(owner.pid, identity).map(|()| true)
    });
    #[cfg(not(any(target_os = "macos", windows)))]
    let identity: Option<Result<bool>> = None;
    match identity {
        Some(Ok(matches)) => OwnerObservation {
            identity_matches: Some(matches),
            ..observation
        },
        Some(Err(error)) => OwnerObservation {
            error: Some(format!("{error:#}")),
            ..observation
        },
        None => observation,
    }
}

pub(super) fn request_result(directory: &Path, request_id: &str) -> Result<Value> {
    observe_snapshot(directory)?.result(directory, &Selector::Request(request_id.to_owned()))
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_result(value: &Value, json: bool) -> Result<()> {
    if json {
        return print_json(value);
    }
    println!("session: {}", value["session"].as_str().unwrap_or("?"));
    if let Some(id) = value["request_id"].as_str() {
        println!("request: {id}");
    }
    println!(
        "state: {}",
        value["request_state"].as_str().unwrap_or("unknown")
    );
    if let Some(message) = value["result"].as_str() {
        println!("\n{}", terminal_safe_text(message, true));
    }
    if let Some(error) = value["error"].as_str() {
        println!("error: {}", terminal_safe_text(error, true));
    }
    Ok(())
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
                value["ok"] = json!(false);
                if value["error"].is_null() {
                    value["error"] = json!(match value["request_state"].as_str() {
                        Some("unavailable") =>
                            "no published result is available for this selection",
                        Some("recovery_required") =>
                            "completion publication requires recovery; run sessions for this workspace, then query again",
                        _ =>
                            "request ended without a published successful result; inspect the session before sending another prompt",
                    });
                }
            }
            if matches!(request.selector, Selector::List) {
                if request.json {
                    print_json(&value)?;
                } else {
                    for event in value["events"].as_array().into_iter().flatten() {
                        println!(
                            "{}\t{}\t{}",
                            event["event_id"].as_str().unwrap_or("?"),
                            event["request_state"].as_str().unwrap_or("?"),
                            event["request_id"].as_str().unwrap_or("legacy")
                        );
                    }
                }
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
                print_json(&json!({"schema_version": 1, "ok": false,
                "session": request.id, "request_id": match &request.selector { Selector::Request(id) => Some(id), _ => None },
                "error": format!("{error:#}"), "result": null }))?;
            }
            Err(error)
        }
    }
}

fn result_value(request: &ResultRequest) -> Result<Value> {
    let directory = session_directory(&request.id)?;
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    let mut last = json!({"schema_version": 1, "ok": true, "session": request.id,
        "request_id": match &request.selector { Selector::Request(id) => Some(id), _ => None },
        "request_state": "busy", "result": null});
    loop {
        let observed = if request.wait {
            Snapshot::read(&directory)
        } else {
            observe_snapshot(&directory)
        };
        match observed {
            Ok(snapshot) => {
                if matches!(request.selector, Selector::List) {
                    let mut events = Vec::new();
                    for path in &snapshot.paths {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .context("invalid event file")?;
                        let mut value =
                            snapshot.result(&directory, &Selector::Event(name.to_owned()))?;
                        value.as_object_mut().unwrap().remove("result");
                        events.push(value);
                    }
                    let mut requests = Vec::new();
                    for receipt in &snapshot.receipts {
                        let mut value = snapshot
                            .result(&directory, &Selector::Request(receipt.request_id.clone()))?;
                        value.as_object_mut().unwrap().remove("result");
                        requests.push(value);
                    }
                    return Ok(
                        json!({"schema_version": 1, "ok": true, "session": request.id, "events": events, "requests": requests,
                        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error}),
                    );
                }
                last = snapshot.result(&directory, &request.selector)?;
                if !request.wait
                    || matches!(
                        last["request_state"].as_str(),
                        Some("completed" | "failed" | "unresolved" | "recovery_required")
                    )
                {
                    return Ok(last);
                }
                let owner = observe_owner(&directory);
                last["owner_process_alive"] = json!(owner.process_alive);
                last["owner"] = json!(owner);
                if owner.process_alive == Some(false) || owner.identity_matches == Some(false) {
                    last["request_state"] = json!("unresolved");
                    last["error"] = json!(
                        "recorded native owner is no longer live; run sessions for this workspace to recover its state, then inspect the request"
                    );
                    return Ok(last);
                }
                if matches!(
                    snapshot.status.state.as_str(),
                    "closed" | "failed" | "exited"
                ) {
                    last["request_state"] = json!("unresolved");
                    return Ok(last);
                }
            }
            Err(error) if request.wait && error.is::<SnapshotBusy>() => (),
            Err(error) => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            last["ok"] = json!(false);
            last["timed_out"] = json!(true);
            last["error"] = json!("waiting timed out; the request was not cancelled or resent");
            return Ok(last);
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

pub(super) fn run_inspect(id: &str, json: bool) -> Result<()> {
    let outcome = inspect_inner(id, json);
    if let Err(error) = &outcome
        && json
    {
        print_json(&json!({"schema_version": 1, "ok": false, "session": id,
            "stored_state": "unknown", "error": format!("{error:#}")}))?;
    }
    outcome
}

fn inspect_inner(id: &str, json: bool) -> Result<()> {
    let directory = session_directory(id)?;
    let snapshot = observe_snapshot(&directory)?;
    let owner = observe_owner(&directory);
    let mut latest = snapshot.result(&directory, &Selector::Latest)?;
    latest.as_object_mut().unwrap().remove("result");
    let request_refs = snapshot
        .receipts
        .iter()
        .map(|receipt| {
            json!({
                "request_id": receipt.request_id, "created_unix_ms": receipt.created_unix_ms, "source": receipt.source,
                "event_id": receipt.event_file, "context_sources": receipt.context_sources,
                "active": snapshot.claim.as_deref() == Some(&receipt.claim_token),
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "schema_version": 1, "ok": true, "session": id, "provider": snapshot.manifest.provider,
        "workspace": snapshot.manifest.workspace, "title": snapshot.manifest.title,
        "stored_state": snapshot.status.state, "generation": snapshot.status.generation,
        "created_unix_ms": snapshot.manifest.created_unix_ms, "updated_unix_ms": snapshot.status.updated_unix_ms,
        "error": snapshot.status.error, "exit_code": snapshot.status.exit_code,
        "configured": {"model": snapshot.manifest.model, "effort": snapshot.manifest.effort,
            "yolo": snapshot.manifest.yolo, "provider_version_at_launch": snapshot.manifest.provider_version},
        "owner_process_alive": owner.process_alive, "owner_identity_verified": owner.identity_matches == Some(true), "owner": owner,
        "recovery_required": snapshot.pending.is_some(), "turn_claimed": snapshot.claim.is_some(),
        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
        "recorded_events": snapshot.paths.len(), "latest_result": latest, "requests": request_refs,
    });
    drop(snapshot);
    if json {
        return print_json(&value);
    }
    println!(
        "session: {id}\nprovider: {}\nstate (stored): {}\nworkspace: {}\nupdated: {}",
        value["provider"].as_str().unwrap_or("?"),
        terminal_safe_text(value["stored_state"].as_str().unwrap_or("unknown"), false),
        terminal_safe_text(value["workspace"].as_str().unwrap_or("?"), false),
        value["updated_unix_ms"]
    );
    println!(
        "owner process alive: {}\nrecovery required: {}",
        value["owner_process_alive"], value["recovery_required"]
    );
    if let Some(error) = value["error"].as_str() {
        println!("error: {}", terminal_safe_text(error, true));
    }
    if let Some(id) = value["latest_result"]["event_id"].as_str() {
        println!("latest result: {id}");
    }
    for request in value["requests"].as_array().into_iter().flatten() {
        println!("request: {}", request["request_id"].as_str().unwrap_or("?"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// `search`: substring lookup over published result bodies. Read-only, budgeted, and scoped
// to one workspace unless the caller removes the filter.
// ---------------------------------------------------------------------------------------

const SEARCH_DEFAULT_LIMIT: usize = 20;
const SEARCH_MAX_LIMIT: usize = 200;
const SEARCH_EVENT_BUDGET: usize = 5_000;
const SEARCH_BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(10);
const SEARCH_EXCERPT_CHARS: usize = 200;

#[derive(Debug)]
pub(super) enum SearchScope {
    Workspace(PathBuf),
    AllWorkspaces,
}

#[derive(Debug)]
pub(crate) struct SearchRequest {
    query: String,
    scope: SearchScope,
    provider: Option<FirstPartyCli>,
    limit: usize,
    json: bool,
}

pub(super) fn parse_search(args: &[String]) -> Result<NativeCommand> {
    let (query, options) = args.split_first().context("search requires a query")?;
    if query.trim().is_empty() {
        bail!("search requires a non-empty query");
    }
    let mut workspace = None;
    let mut all_workspaces = false;
    let mut provider = None;
    let mut limit = None;
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
            "--json" => set_flag_once(&mut json, "--json")?,
            other => bail!("unknown search option: {other}"),
        }
        index += 1;
    }
    let scope = match (workspace, all_workspaces) {
        (Some(_), true) => bail!("search accepts only one of --workspace or --all-workspaces"),
        (None, true) => SearchScope::AllWorkspaces,
        // Same canonical form that `sessions --workspace` compares against the manifest.
        (Some(path), false) => SearchScope::Workspace(path.canonicalize().or_else(|_| {
            if path.is_absolute() {
                Ok(path)
            } else {
                std::env::current_dir().map(|cwd| cwd.join(path))
            }
        })?),
        (None, false) => SearchScope::Workspace(
            std::env::current_dir()
                .and_then(|cwd| cwd.canonicalize())
                .context(
                    "failed to resolve the current workspace; pass --workspace or --all-workspaces",
                )?,
        ),
    };
    Ok(NativeCommand::Search(SearchRequest {
        query: query.clone(),
        scope,
        provider,
        limit: limit.unwrap_or(SEARCH_DEFAULT_LIMIT),
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
    created_unix_ms: u128,
    excerpt: String,
    result_command: String,
}

#[derive(Serialize)]
struct IncompleteReason {
    session: Option<String>,
    reason: String,
}

struct SearchScan {
    started: Instant,
    events_read: usize,
    bytes_read: u64,
    sessions_scanned: usize,
    hits: Vec<SearchHit>,
    reasons: Vec<IncompleteReason>,
}

impl SearchScan {
    /// Names the budget that is already exhausted. Checked before every read so a scan
    /// never reads past its limits.
    fn exhausted_budget(&self) -> Option<String> {
        if self.events_read >= SEARCH_EVENT_BUDGET {
            Some(format!(
                "event budget of {SEARCH_EVENT_BUDGET} reads exhausted"
            ))
        } else if self.bytes_read >= SEARCH_BYTE_BUDGET {
            Some(format!(
                "byte budget of {} MiB exhausted",
                SEARCH_BYTE_BUDGET / (1024 * 1024)
            ))
        } else if self.started.elapsed() >= SEARCH_TIME_BUDGET {
            Some(format!(
                "time budget of {} s exhausted",
                SEARCH_TIME_BUDGET.as_secs()
            ))
        } else {
            None
        }
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

fn excerpt(message: &str, match_start: usize, match_chars: usize) -> String {
    let total = message.chars().count();
    if total <= SEARCH_EXCERPT_CHARS {
        return terminal_safe_text(message, false);
    }
    // Both cut markers count toward the character limit.
    let body = SEARCH_EXCERPT_CHARS - 2;
    let start = match_start
        .saturating_sub(body.saturating_sub(match_chars.min(body)) / 2)
        .min(total - body);
    let end = start + body;
    let window = message.chars().skip(start).take(body).collect::<String>();
    let mut text = String::new();
    if start > 0 {
        text.push('…');
    }
    text.push_str(&terminal_safe_text(&window, false));
    if end < total {
        text.push('…');
    }
    text
}

enum SessionScan {
    Done,
    Budget(String),
}

fn search_session(
    request: &SearchRequest,
    query_lower: &str,
    directory: &Path,
    id: &str,
    scan: &mut SearchScan,
) -> Result<SessionScan, String> {
    // The manifest alone decides scope, so out-of-scope sessions are never snapshotted.
    let manifest = read_manifest(directory).map_err(|error| format!("{error:#}"))?;
    if let SearchScope::Workspace(workspace) = &request.scope
        && workspace != &manifest.workspace
    {
        return Ok(SessionScan::Done);
    }
    if request
        .provider
        .is_some_and(|provider| provider.as_str() != manifest.provider)
    {
        return Ok(SessionScan::Done);
    }
    let snapshot = observe_snapshot(directory).map_err(|error| format!("{error:#}"))?;
    scan.sessions_scanned += 1;
    for path in &snapshot.paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !snapshot.published(name) {
            continue;
        }
        if let Some(budget) = scan.exhausted_budget() {
            return Ok(SessionScan::Budget(budget));
        }
        scan.events_read += 1;
        let event: SessionEvent = match read_regular_text_if_present(path) {
            Ok(Some(text)) => {
                scan.bytes_read += text.len() as u64;
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
            Err(error) => {
                scan.incomplete(Some(id), format!("{name}: {error:#}"));
                continue;
            }
        };
        if event.error.is_some() {
            continue;
        }
        let Some(start) = find_case_insensitive(&event.message, query_lower) else {
            continue;
        };
        let request_id = snapshot
            .receipt_for_event(name)
            .map(|receipt| receipt.request_id.clone());
        let result_command = match &request_id {
            Some(request_id) => format!("agent-bridge result {id} --request {request_id} --json"),
            None => format!("agent-bridge result {id} --event {name} --json"),
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
    Ok(SessionScan::Done)
}

fn search_value(request: &SearchRequest) -> Result<Value> {
    let root = state_root()?;
    let mut ids = Vec::new();
    match fs::read_dir(&root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.context("failed to read the native state root")?;
                let id = entry.file_name().to_string_lossy().into_owned();
                if valid_session_id(&id) && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    ids.push(id);
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
    let mut scan = SearchScan {
        started: Instant::now(),
        events_read: 0,
        bytes_read: 0,
        sessions_scanned: 0,
        hits: Vec::new(),
        reasons: Vec::new(),
    };
    for id in &ids {
        let budget = match scan.exhausted_budget() {
            Some(budget) => Some(budget),
            None => match search_session(request, &query_lower, &root.join(id), id, &mut scan) {
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
    Ok(json!({
        "schema_version": 1, "ok": true, "query": request.query,
        "filters": {
            "workspace": workspace, "all_workspaces": all_workspaces,
            "provider": request.provider.map(FirstPartyCli::as_str),
        },
        "limit": request.limit, "hits": scan.hits, "truncated": truncated,
        "incomplete": !scan.reasons.is_empty(), "incomplete_reasons": scan.reasons,
        "scanned": { "sessions": scan.sessions_scanned, "events": scan.events_read },
    }))
}

pub(super) fn run_search(request: SearchRequest) -> Result<()> {
    let value = match search_value(&request) {
        Ok(value) => value,
        Err(error) => {
            if request.json {
                print_json(&json!({"schema_version": 1, "ok": false,
                    "query": request.query, "error": format!("{error:#}"), "hits": []}))?;
            }
            return Err(error);
        }
    };
    if request.json {
        return print_json(&value);
    }
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
}
