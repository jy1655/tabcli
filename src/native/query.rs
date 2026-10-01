//! Read-only views of durable session records. Never recover, send, or close here.
use super::*;
use serde_json::{Value, json};

mod timeline;
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
    pub(super) launch: Option<launch::Record>,
    /// The publication predicate's bounded read of the journal's event file, when a journal
    /// exists and the snapshot performed the read (see `published` and
    /// [`PublicationRead`]); a search defers it to the event's scan position and keeps
    /// `None` here.
    pending_event: Option<JournaledEventRead>,
    _lock: Option<File>,
}

#[cfg(test)]
type SnapshotHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs inside every snapshot attempt, after its state records and any publication
    /// read and before its consistency check, so a test can change a state record there
    /// and force the attempt to be retried.
    static BEFORE_CONSISTENCY_CHECK: std::cell::RefCell<Option<SnapshotHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `hook` for the snapshots `run` takes on this thread (see
/// [`BEFORE_CONSISTENCY_CHECK`]) and removes it afterwards.
#[cfg(test)]
pub(super) fn with_snapshot_hook<T>(
    hook: impl FnMut(&Path) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    BEFORE_CONSISTENCY_CHECK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
    let outcome = run();
    BEFORE_CONSISTENCY_CHECK.with(|cell| *cell.borrow_mut() = None);
    outcome
}

fn before_consistency_check(directory: &Path) {
    #[cfg(test)]
    BEFORE_CONSISTENCY_CHECK.with(|cell| {
        if let Some(hook) = cell.borrow_mut().as_mut() {
            hook(directory);
        }
    });
    #[cfg(not(test))]
    let _ = directory;
}

/// When a snapshot decides the publication of a journaled event.
#[derive(Clone, Copy, Debug)]
enum PublicationRead {
    /// Inside the snapshot, comparing at most this many bytes of the event with its
    /// journal. Every non-search query uses the fixed [`EVENT_READ_LIMIT`], so it
    /// publishes exactly what an uncontended read would publish, however many attempts a
    /// busy retry took.
    Within(u64),
    /// Not inside the snapshot: a search reads the journaled event only when its scan
    /// reaches the event's position in filename order, within the budget left at that
    /// moment, so the journal's presence never changes which earlier records the scan
    /// examines, and no retry of the snapshot costs a read. Until the caller performs
    /// that read, the journaled event counts as unpublished.
    Deferred,
}

pub(super) fn optional_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    read_regular_text_if_present(path)?
        .map(|text| {
            serde_json::from_str(&text)
                .with_context(|| format!("invalid JSON in {}", path.display()))
        })
        .transpose()
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
    pub(super) fn read(directory: &Path) -> Result<Self> {
        Self::read_with(directory, PublicationRead::Within(EVENT_READ_LIMIT))
    }

    /// [`Snapshot::read`] whose publication check reads at most `event_limit` bytes of a
    /// journaled event.
    #[cfg(test)]
    pub(super) fn read_within(directory: &Path, event_limit: u64) -> Result<Self> {
        Self::read_with(directory, PublicationRead::Within(event_limit))
    }

    /// [`Snapshot::read`] deciding a journaled event's publication as `publication` says.
    fn read_with(directory: &Path, publication: PublicationRead) -> Result<Self> {
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
        let state_files = [
            "status.json",
            TURN_CLAIM_FILE,
            TURN_COMPLETION_FILE,
            launch::FILE,
        ];
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
        let pending_event = match (&pending, publication) {
            (Some(pending), PublicationRead::Within(event_limit)) => {
                validate_pending_completion(pending)?;
                // The predicate validates `events` before it opens anything under it, so
                // a link planted there is rejected instead of followed by this read.
                Some(journaled_event_state_within(
                    directory,
                    pending,
                    event_limit,
                )?)
            }
            (Some(pending), PublicationRead::Deferred) => {
                validate_pending_completion(pending)?;
                None
            }
            (None, _) => None,
        };
        before_consistency_check(directory);
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
            launch: launch::read(directory)?,
            pending_event,
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

    /// The completion journal naming `event`, when one exists.
    fn journal_for(&self, event: &str) -> Option<&PendingTurnCompletion> {
        self.pending
            .as_ref()
            .filter(|pending| pending.event_file == event)
    }

    fn published(&self, event: &str) -> bool {
        // A journaled event is published exactly when its file already holds the journal's
        // bytes: every lifecycle path (recovery, close, interrupted close) keeps such an
        // event, and none publishes a journaled event that was never written, differs, or
        // could not be compared within the read limit. The claim does not decide it: a
        // close releases the claim before it removes the journal, so the journal outlives
        // every interruption that could otherwise hide a committed event. A snapshot that
        // deferred the comparison has no verdict yet, and no verdict publishes nothing.
        if self.journal_for(event).is_some() {
            return self
                .pending_event
                .as_ref()
                .is_some_and(|read| read.state == JournaledEventState::Committed);
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
            if event.error.is_some() {
                "failed"
            } else {
                "completed"
            }
        } else if name
            .is_some_and(|name| self.pending.as_ref().is_some_and(|p| p.event_file == name))
        {
            "recovery_required"
        } else if launch_failure.is_some() {
            if self.status.state == "failed" {
                "failed"
            } else {
                "unresolved"
            }
        } else if receipt.is_some_and(|r| self.claim.as_deref() == Some(&r.claim_token)) {
            "pending"
        } else if receipt.is_some() {
            "unresolved"
        } else {
            "unavailable"
        };
        let (elapsed, elapsed_reason) = observed_elapsed(receipt, event.as_ref());
        Ok(json!({
            "bridge_observed_elapsed_ms": elapsed, "bridge_observed_elapsed_reason": elapsed_reason,
            "schema_version": 1, "ok": true, "session": self.manifest.id,
            "provider": self.manifest.provider, "workspace": self.manifest.workspace,
            "request_id": receipt.map(|r| &r.request_id), "event_id": name,
            "context_sources": receipt.map(|r| r.context_sources.as_slice()).unwrap_or_default(),
            "request_state": state, "session_state": self.status.state,
            "result": event.as_ref().map(|e| &e.message),
            "error": event.as_ref().and_then(|e| e.error.as_ref()).or_else(||
                if event.is_none() { launch_failure.as_ref().map(|(_, detail)| detail) } else { None }),
            "session_error": self.status.error,
            "provider_session_id": event.as_ref().and_then(|e| e.provider_session_id.as_ref()),
            "turn_id": event.as_ref().and_then(|e| e.turn_id.as_ref()),
            "created_unix_ms": event.as_ref().map(|e| e.created_unix_ms),
            "recovery_required": self.pending.is_some(),
            "unreadable_requests": self.unreadable_requests, "request_index_error": self.request_index_error,
        }))
    }
}

/// The snapshot every non-search query observes: its publication check compares a
/// journaled event within the fixed [`EVENT_READ_LIMIT`] on every attempt.
pub(super) fn observe_snapshot(directory: &Path) -> Result<Snapshot> {
    observe_snapshot_with(directory, PublicationRead::Within(EVENT_READ_LIMIT))
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
fn observe_snapshot_with(directory: &Path, publication: PublicationRead) -> Result<Snapshot> {
    let deadline = Instant::now() + snapshot_retry_window();
    loop {
        match Snapshot::read_with(directory, publication) {
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
    println!("{}", elapsed_text(value));
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
                            "{}\t{}\t{}\t{}",
                            event["event_id"].as_str().unwrap_or("?"),
                            event["request_state"].as_str().unwrap_or("?"),
                            event["request_id"].as_str().unwrap_or("legacy"),
                            elapsed_text(event)
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
    result_value_in(&state_root()?, request)
}

/// The `result` command's value over one state root, including `--wait`.
pub(super) fn result_value_in(root: &Path, request: &ResultRequest) -> Result<Value> {
    let directory = session_directory_in(root, &request.id)?;
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    let mut last = json!({"schema_version": 1, "ok": true, "session": request.id,
        "request_id": match &request.selector { Selector::Request(id) => Some(id), _ => None },
        "request_state": "busy", "result": null,
        "bridge_observed_elapsed_ms": null, "bridge_observed_elapsed_reason": "no_published_result"});
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

pub(super) fn run_inspect(
    id: &str,
    json: bool,
    timeline: bool,
    request: Option<&str>,
) -> Result<()> {
    let outcome = if timeline {
        timeline::run(id, json, request)
    } else {
        inspect_inner(id, json)
    };
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
    let value = inspect_value(&directory, id)?;
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
    if let Some(source) = value["resumed_from"]["session"].as_str() {
        println!("resumed from: {}", terminal_safe_text(source, false));
    }
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

pub(super) fn inspect_value(directory: &Path, id: &str) -> Result<Value> {
    let snapshot = observe_snapshot(directory)?;
    let owner = observe_owner(directory);
    let resumed_from = read_resumed_from(directory)?;
    let mut latest = snapshot.result(directory, &Selector::Latest)?;
    latest.as_object_mut().unwrap().remove("result");
    let request_refs = snapshot
        .receipts
        .iter()
        .map(|receipt| {
            let (elapsed, elapsed_reason) = match snapshot.event(directory, &receipt.event_file) {
                Ok(event) => observed_elapsed(Some(receipt), event.as_ref()),
                Err(_) => (None, Some("unreadable_result")),
            };
            json!({
                "request_id": receipt.request_id, "created_unix_ms": receipt.created_unix_ms, "source": receipt.source,
                "event_id": receipt.event_file, "context_sources": receipt.context_sources,
                "active": snapshot.claim.as_deref() == Some(&receipt.claim_token),
                "bridge_observed_elapsed_ms": elapsed,
                "bridge_observed_elapsed_reason": elapsed_reason,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "schema_version": 1, "ok": true, "session": id, "provider": snapshot.manifest.provider,
        "workspace": snapshot.manifest.workspace, "title": snapshot.manifest.title,
        "stored_state": snapshot.status.state, "generation": snapshot.status.generation,
        "created_unix_ms": snapshot.manifest.created_unix_ms, "updated_unix_ms": snapshot.status.updated_unix_ms,
        "error": snapshot.status.error, "exit_code": snapshot.status.exit_code,
        "configured": {"model": snapshot.manifest.model, "effort": snapshot.manifest.effort,
            "yolo": snapshot.manifest.yolo, "provider_version_at_launch": snapshot.manifest.provider_version},
        "resumed_from": resumed_from,
        "workspace_consent": consent::observe(directory),
        "owner_process_alive": owner.process_alive, "owner_identity_verified": owner.identity_matches == Some(true), "owner": owner,
        "recovery_required": snapshot.pending.is_some(), "turn_claimed": snapshot.claim.is_some(),
        "unreadable_requests": snapshot.unreadable_requests, "request_index_error": snapshot.request_index_error,
        "recorded_events": snapshot.paths.len(), "latest_result": latest, "requests": request_refs,
    }))
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
/// Limit on the displayed excerpt: it is measured after control characters are escaped,
/// so an excerpt never exceeds this many characters however it was sanitised.
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
            if args.iter().skip(1).any(|option| option == "--json") {
                print_json(&search_error_value(
                    args.first().filter(|query| !query.trim().is_empty()),
                    &error,
                ))?;
            }
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
    json!({"schema_version": 1, "ok": false, "query": query,
        "error": format!("{error:#}"), "hits": []})
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

/// Reads one event without exceeding the remaining byte budget. `Ok(None)` means the
/// file disappeared during the scan; `Err(Ok(bytes))` means the file is too large for the
/// budget and was not consumed; `Err(Err(error))` is an I/O failure.
fn read_event_within_budget(path: &Path, remaining: u64) -> Result<Option<String>, Result<u64>> {
    use std::io::Read as _;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Err(
                anyhow::Error::new(error).context(format!("failed to inspect {}", path.display()))
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Err(anyhow::anyhow!(
            "refusing non-regular session file: {}",
            path.display()
        )));
    }
    if metadata.len() > remaining {
        return Err(Ok(metadata.len()));
    }
    // The file may have grown since the metadata read: never read past the budget.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Err(
                anyhow::Error::new(error).context(format!("failed to read {}", path.display()))
            ));
        }
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.take(remaining + 1).read_to_end(&mut bytes) {
        return Err(Err(
            anyhow::Error::new(error).context(format!("failed to read {}", path.display()))
        ));
    }
    if bytes.len() as u64 > remaining {
        return Err(Ok(bytes.len() as u64));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The shared event listing turns a missing or non-directory `events` path into an
/// empty list; a search must not present that damage as "no results".
fn check_events_directory(directory: &Path) -> Result<(), String> {
    require_events_directory(directory).map_err(|error| format!("{error:#}"))
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
    if let Some(budget) = scan.exhausted_budget() {
        return Ok(SessionScan::Budget(budget));
    }
    // `events` is validated before anything is read through it, and again after the
    // snapshot listed it.
    check_events_directory(directory)?;
    // The snapshot reads no event: a journaled event's publication is decided below, at
    // the event's own position in the scan, so a busy retry costs nothing and the
    // journal's presence never changes which records the scan examines before it.
    let snapshot = observe_snapshot_with(directory, PublicationRead::Deferred)
        .map_err(|error| format!("{error:#}"))?;
    scan.sessions_scanned += 1;
    check_events_directory(directory)?;
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
            Some(pending) => {
                // The publication comparison is the journaled event's one read: it is
                // charged like any other, and a committed event is searched from the
                // bytes it compared. A record the budget cannot hold stops the scan with
                // the same reason an ordinary record would, so the result is the same
                // once the journal is gone and the event is read the ordinary way.
                let read = journaled_event_state_within(directory, pending, remaining)
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
            None => read_event_within_budget(path, remaining).inspect(|text| {
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
        if event.error.is_some() {
            continue;
        }
        let Some(start) = find_case_insensitive(&event.message, query_lower) else {
            continue;
        };
        let request_id = receipt.map(|receipt| receipt.request_id.clone());
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
    search_value_in(&state_root()?, request)
}

/// The `search` command's value over one state root.
pub(super) fn search_value_in(root: &Path, request: &SearchRequest) -> Result<Value> {
    // The clock starts before enumeration so slow roots count against the budget too.
    let mut scan = SearchScan::new(request.byte_budget);
    let mut ids = Vec::new();
    match fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.context("failed to read the native state root")?;
                let id = entry.file_name().to_string_lossy().into_owned();
                if !valid_session_id(&id) {
                    continue;
                }
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => ids.push(id),
                    Ok(_) => (),
                    // An unreadable entry may be a session; it is never silently dropped.
                    Err(error) => scan.incomplete(
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
                print_json(&search_error_value(Some(&request.query), &error))?;
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
            read_event_within_budget(&path, 10).unwrap().unwrap(),
            "0123456789"
        );
        assert_eq!(read_event_within_budget(&path, 9).unwrap_err().unwrap(), 10);
        assert!(
            read_event_within_budget(&directory.path().join("missing.json"), 10)
                .unwrap()
                .is_none()
        );
        let error = read_event_within_budget(directory.path(), 10)
            .unwrap_err()
            .unwrap_err();
        assert!(error.to_string().contains("non-regular"), "{error}");
        assert!(check_events_directory(directory.path()).is_err());
        fs::write(directory.path().join("events"), "x").unwrap();
        assert_eq!(
            check_events_directory(directory.path()).unwrap_err(),
            "events is not a directory"
        );
    }
}

#[cfg(test)]
mod elapsed_tests {
    use super::*;

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
}
