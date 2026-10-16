//! Reopen of a closed Session: source reservation, launch refusal, and cleanup settlement.
//! Provider adapters retain conversation-holder evidence; terminal ownership retains Close authority.
use super::doctor::{Availability, Check};
use super::*;
use crate::native::session::primitives::{delegation_source, unix_ms, valid_session_id};
use crate::native::session::schema::{
    ProviderProcessRecord, RecordedReopenRefusal, SessionEvent, SessionManifest,
};
use crate::native::session::turn::TURN_CLAIM_SEQUENCE;
use serde_json::{Value, json};

// Where a reopened session's provider conversation came from: the closed Bridge session and
// the event whose provider session id was passed to the provider's official resume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ResumedFrom {
    pub(super) session: String,
    pub(super) provider_session_id: String,
    pub(super) event_id: String,
}

// Reopen provenance is an optional `resumed_from` object stored in the schema-1 manifest
// beside the fields every reader knows. It is read through this sibling type so a reader
// that does not know the field keeps parsing the manifest unchanged.
#[derive(Debug, Default, Deserialize)]
struct ReopenProvenance {
    #[serde(default)]
    resumed_from: Option<ResumedFrom>,
}

// The record a winning reopen leaves in its closed source. `reopened_by` is filled once the
// new session exists; until then the marker still excludes every other reopen attempt.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReopenMarker {
    schema: u32,
    claim: String,
    provider_session_id: String,
    #[serde(default)]
    reopened_by: Option<String>,
    created_unix_ms: u128,
}

// A reopen that fails a pre-launch gate. The gate name reaches the JSON response so a caller
// can tell a refused reopen from a launch or delivery failure of the new session.
#[derive(Debug)]
struct ReopenRefusal {
    gate: &'static str,
    detail: String,
}

impl std::fmt::Display for ReopenRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reopen refused ({}): {}", self.gate, self.detail)
    }
}

impl std::error::Error for ReopenRefusal {}

fn reopen_refusal(gate: &'static str, detail: String) -> anyhow::Error {
    anyhow::Error::new(ReopenRefusal { gate, detail })
}

pub(super) fn reopen_refusal_gate(error: &anyhow::Error) -> Option<&'static str> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ReopenRefusal>())
        .map(|refusal| refusal.gate)
}

pub(super) const REOPEN_LAUNCH_GATE: &str = "provider-unsupported";
const REOPEN_CONFLICT_GATE: &str = "reopen-conflict";
const REOPEN_VERIFICATION_FAILED_GATE: &str = "reopen-verification-failed";
// Launch admission refused the source workspace before the new session existed.
const REOPEN_WORKSPACE_REFUSED_GATE: &str = "workspace-refused";

// The gates a reopen can fail after its session exists. Each one leaves the new session
// failed with no prompt delivered. The source's reopen marker is released again only once
// the refused launch provably cannot hold the conversation (`RefusedLaunchCleanup`).
const REOPEN_POST_CREATION_GATES: [&str; 3] = [
    REOPEN_LAUNCH_GATE,
    REOPEN_CONFLICT_GATE,
    REOPEN_VERIFICATION_FAILED_GATE,
];

// Only launch gates persist refusals across processes; a later tell refusal must never
// authorize retry of an uncertain initial delivery.
const REOPEN_REFUSAL_LAUNCH_PHASE: &str = "launch";

// Cleanup is a diagnostic for doctor and the next reopen; only live records decide release.
const REOPEN_REFUSAL_CLEANUP_PENDING: &str = "pending";

// Records a launch-phase gate refusal that happened after the new session existed, then
// returns the typed refusal. Only the launch phase writes this record; follow-up refusals
// go through `record_follow_up_refusal`, which touches the session status alone.
pub(super) fn record_reopen_refusal(
    directory: &Path,
    gate: &'static str,
    detail: String,
) -> anyhow::Error {
    let record = RecordedReopenRefusal {
        schema: 2,
        phase: REOPEN_REFUSAL_LAUNCH_PHASE.to_owned(),
        gate: gate.to_owned(),
        detail: detail.clone(),
        created_unix_ms: unix_ms(),
        cleanup: None,
        cleanup_detail: None,
    };
    if let Err(error) = Store::open_unchecked(directory).write_reopen_refusal(&record) {
        return reopen_refusal(
            gate,
            format!("{detail}; the refusal record could not be written: {error:#}"),
        );
    }
    reopen_refusal(gate, detail)
}

// The launch-phase refusal recorded in a reopened session, if there is one. A record of any
// other schema or phase is not a launch refusal and yields nothing, so the caller treats the
// launch as not refused.
fn read_reopen_launch_refusal(directory: &Path) -> Option<RecordedReopenRefusal> {
    let text = Reader::open_unchecked(directory)
        .record(CoreRecord::ReopenRefusal)
        .text()
        .ok()
        .flatten()?;
    let record: RecordedReopenRefusal = serde_json::from_str(&text).ok()?;
    (record.schema == 2 && record.phase == REOPEN_REFUSAL_LAUNCH_PHASE).then_some(record)
}

fn read_reopen_refusal_gate(directory: &Path) -> Option<String> {
    read_reopen_launch_refusal(directory).map(|record| record.gate)
}

// Notes in the launch refusal record that the marker settlement could not establish that
// the refused launch is gone. A missing record (its write failed when the refusal was
// decided) leaves nothing to annotate; the retention itself does not depend on the note.
fn record_reopen_refusal_cleanup_pending(directory: &Path, reason: &str) -> Result<()> {
    let Some(mut record) = read_reopen_launch_refusal(directory) else {
        return Ok(());
    };
    record.cleanup = Some(REOPEN_REFUSAL_CLEANUP_PENDING.to_owned());
    record.cleanup_detail = Some(reason.to_owned());
    Store::open_unchecked(directory).write_reopen_refusal(&record)
}

// Why a recorded provider process is known to be gone. Identity mismatches are only
// observable where the reopen slice runs (native Windows); other targets never build one.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessGone {
    // Its pid is no longer alive.
    Exited,
    // Its pid is alive but belongs to a different process: the pid was reused after the
    // recorded process exited (Windows creation time or executable path differ).
    IdentityMismatch(String),
}

// What can be observed about a recorded provider process now.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessObservation {
    Gone(ProviderProcessGone),
    // The pid is alive and, where an identity was recorded, still carries it.
    Alive,
    // Its identity could not be inspected; PID-only liveness cannot establish whether
    // the recorded incarnation survived, exited, or was reused.
    Unknown(String),
}

// A verified PID reuse releases the marker; an inspection failure retains it.
fn observe_provider_process(record: &ProviderProcessRecord) -> ProviderProcessObservation {
    #[cfg(windows)]
    // B2: an exited provider whose object is still retained is Gone(Exited), as the former
    // typed check also concluded through its liveness fallback; no liveness bool is consulted.
    return observe_provider_process_with(record, terminal::observe_windows_process_identity);
    #[cfg(not(windows))]
    {
        use crate::native::process_identity::{self, ProcessObservation};
        match process_identity::classify::<()>(
            record.pid,
            None,
            || process_is_alive(record.pid),
            || Ok(None),
        ) {
            ProcessObservation::Live(_) => ProviderProcessObservation::Alive,
            ProcessObservation::Gone(_) => {
                ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
            }
            ProcessObservation::Unknown(reason) => ProviderProcessObservation::Unknown(reason),
        }
    }
}

#[cfg(any(windows, test))]
fn observe_provider_process_with(
    record: &ProviderProcessRecord,
    observe: impl FnOnce(u32) -> Result<Option<terminal::WindowsProcessIdentity>>,
) -> ProviderProcessObservation {
    use crate::native::process_identity::{self, Gone, ProcessObservation};
    use terminal::WindowsIncarnation;
    let recorded = record
        .windows_process_identity
        .clone()
        .map(WindowsIncarnation);
    // Keep the published Windows mismatch vocabulary while classify owns the decision.
    let mut mismatch = "Windows console process executable identity changed";
    let observation = process_identity::classify(
        record.pid,
        recorded.as_ref(),
        || process_is_alive(record.pid),
        || {
            let live = observe(record.pid)?;
            if let (Some(recorded), Some(live)) = (&recorded, &live)
                && recorded.0.creation_time != live.creation_time
            {
                mismatch = "Windows console process id was reused";
            }
            Ok(live.map(WindowsIncarnation))
        },
    );
    match observation {
        ProcessObservation::Live(_) => ProviderProcessObservation::Alive,
        ProcessObservation::Gone(Gone::Exited) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
        }
        ProcessObservation::Gone(Gone::Reused(_)) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::IdentityMismatch(mismatch.into()))
        }
        ProcessObservation::Unknown(reason) => ProviderProcessObservation::Unknown(reason),
    }
}

// Release only when the provider is verified gone or never spawned. Wrapper death and
// a closed surface cannot rule out an unregistered provider that still holds the conversation.
#[derive(Debug, PartialEq)]
enum RefusedLaunchCleanup {
    // The pre-spawn recheck refused and no provider process was recorded: the launch
    // wrapper started none.
    NoProcessSpawned,
    // The provider process the launch wrapper recorded is verified gone: its pid is dead
    // or the pid is alive under a different identity. `surface_closed` adds that the
    // refused session's surface was consumed by a close; it is reported, never relied on.
    ProviderProcessGone {
        pid: u32,
        evidence: ProviderProcessGone,
        surface_closed: bool,
    },
    // Neither of the above can be established from the session's records.
    Pending(String),
}

impl RefusedLaunchCleanup {
    fn releases_marker(&self) -> bool {
        !matches!(self, Self::Pending(_))
    }
}

impl std::fmt::Display for RefusedLaunchCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProcessSpawned => write!(formatter, "no provider process was spawned"),
            Self::ProviderProcessGone {
                pid,
                evidence,
                surface_closed,
            } => {
                write!(formatter, "provider process {pid} is verified gone (")?;
                match evidence {
                    ProviderProcessGone::Exited => write!(formatter, "it has exited")?,
                    ProviderProcessGone::IdentityMismatch(reason) => {
                        write!(
                            formatter,
                            "the pid now belongs to another process: {reason}"
                        )?;
                    }
                }
                write!(formatter, ")")?;
                if *surface_closed {
                    write!(formatter, " and the refused session's surface was closed")?;
                }
                Ok(())
            }
            Self::Pending(reason) => {
                write!(
                    formatter,
                    "the refused launch may still hold the conversation: {reason}"
                )
            }
        }
    }
}

// Read-only: ready/working sessions are not refused launches. A pre-spawn refusal proves
// no spawn only without a provider record; an existing record must be verified.
fn refused_launch_cleanup(refused_directory: &Path, gate: &str) -> Result<RefusedLaunchCleanup> {
    refused_launch_cleanup_with(refused_directory, gate, observe_provider_process)
}

fn refused_launch_cleanup_with(
    refused_directory: &Path,
    gate: &str,
    observe: impl FnOnce(&ProviderProcessRecord) -> ProviderProcessObservation,
) -> Result<RefusedLaunchCleanup> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    // The refused session's own status decides whether cleanup is even possible, so it is
    // read like the other evidence here: a link at `status.json` is refused, not followed.
    let status_path = Reader::open_unchecked(refused_directory)
        .record(CoreRecord::Status)
        .path()
        .to_owned();
    let state = Reader::open_unchecked(refused_directory)
        .regular_status_if_present()?
        .with_context(|| format!("failed to read {}", status_path.display()))?
        .state;
    if state.accepts_prompt() || state == SessionState::Working {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state}"
        )));
    }
    let record = RecordReader::at(
        Reader::open_unchecked(refused_directory)
            .record(CoreRecord::ProviderProcess)
            .path(),
    )
    .optional_json::<ProviderProcessRecord>()?;
    let Some(record) = record else {
        if gate == REOPEN_LAUNCH_GATE {
            return Ok(RefusedLaunchCleanup::NoProcessSpawned);
        }
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "refused session {refused_session} is {state} with no provider process record ({}), so the provider process it spawned cannot be verified gone",
            CoreRecord::ProviderProcess.name()
        )));
    };
    if record.schema != 1 || record.managed_session_id != refused_session {
        return Ok(RefusedLaunchCleanup::Pending(format!(
            "the provider process record of refused session {refused_session} names {:?} (schema {})",
            record.managed_session_id, record.schema
        )));
    }
    let evidence = match observe(&record) {
        ProviderProcessObservation::Gone(evidence) => evidence,
        ProviderProcessObservation::Alive => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} is still running",
                record.pid
            )));
        }
        ProviderProcessObservation::Unknown(error) => {
            return Ok(RefusedLaunchCleanup::Pending(format!(
                "provider process {} of refused session {refused_session} could not be verified: {error}",
                record.pid
            )));
        }
    };
    let surface_closed = state == SessionState::Closed
        && RecordReader::at(
            Reader::open_unchecked(refused_directory)
                .record(CoreRecord::TerminalClosed)
                .path(),
        )
        .is_regular_file()?
        && Reader::open_unchecked(refused_directory)
            .regular_closed_if_present()?
            .is_some_and(|closed| closed.state == SessionState::Closed);
    Ok(RefusedLaunchCleanup::ProviderProcessGone {
        pid: record.pid,
        evidence,
        surface_closed,
    })
}

// Which boundary a resumed session's holder check runs at. The provider grants no
// exclusive hold on a conversation, so the check is best-effort detection repeated at every
// point Bridge is about to act on the conversation, never a reservation of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ResumedHolderCheck {
    // In the initial-prompt readiness window: the reopened process exists and has not
    // received a prompt. The adapter waits for the provider's own registration of it.
    AfterLaunch,
    // Immediately before the initial prompt is sent to the already registered process. The
    // adapter answers from the registry as it is now.
    BeforeInitialDelivery,
    // Immediately before a later `tell` is sent. Same answer as the initial check, but a
    // refusal here is a follow-up refusal: it is never persisted as a launch refusal.
    BeforeFollowUp,
}

impl ResumedHolderCheck {
    // The launch-phase checks persist their refusal for the reopen command; see
    // `REOPEN_REFUSAL_LAUNCH_PHASE`.
    fn persists_refusal(self) -> bool {
        match self {
            Self::AfterLaunch | Self::BeforeInitialDelivery => true,
            Self::BeforeFollowUp => false,
        }
    }
}

// Other live holders refuse as conflict; incomplete verification also refuses delivery.
// Persist launch refusals only; foreign resumes between checks remain undetected.
pub(super) fn verify_reopened_conversation_exclusive(
    provider: FirstPartyCli,
    directory: &Path,
    resumed_from: Option<&ResumedFrom>,
    deadline: Instant,
    check: ResumedHolderCheck,
) -> Result<()> {
    let Some(resumed_from) = resumed_from else {
        return Ok(());
    };
    let refuse = |gate: &'static str, detail: String| {
        if check.persists_refusal() {
            record_reopen_refusal(directory, gate, detail)
        } else {
            reopen_refusal(gate, detail)
        }
    };
    let others = match provider::reopen(provider).and_then(|support| {
        let context = provider::ResumedSessionContext {
            directory,
            provider_session_id: &resumed_from.provider_session_id,
        };
        match check {
            ResumedHolderCheck::AfterLaunch => {
                support.other_holders_after_registration(context, deadline)
            }
            ResumedHolderCheck::BeforeInitialDelivery | ResumedHolderCheck::BeforeFollowUp => {
                support.other_holders_now(context)
            }
        }
    }) {
        Ok(others) => others,
        Err(error) => {
            return Err(refuse(
                REOPEN_VERIFICATION_FAILED_GATE,
                format!(
                    "could not verify that the reopened {} conversation {} has no other live holder: {error:#}; no prompt was delivered",
                    provider.as_str(),
                    resumed_from.provider_session_id
                ),
            ));
        }
    };
    if others.is_empty() {
        return Ok(());
    }
    Err(refuse(
        REOPEN_CONFLICT_GATE,
        format!(
            "{} conversation {} is also held by live {} process(es) {}; no prompt was delivered",
            provider.as_str(),
            resumed_from.provider_session_id,
            provider.as_str(),
            others
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

// A failed holder check closes only the new surface and reports its close outcome.
// Other launch failures only fail the new session; the source keeps its tombstone.
pub(super) fn close_surface_after_reopen_verification_failure(
    directory: &Path,
    id: &str,
    error: anyhow::Error,
    deadline: std::time::Instant,
) -> anyhow::Error {
    close_surface_after_reopen_verification_failure_with(id, error, |detected| {
        close_session_surface(
            directory,
            id,
            Some(format!("{detected:#}")),
            deadline.min(crate::native::session::close::budget::close_deadline()),
        )
    })
}

// `close` receives the detected refusal so the closed status can keep it as its reason.
fn close_surface_after_reopen_verification_failure_with(
    id: &str,
    error: anyhow::Error,
    close: impl FnOnce(&anyhow::Error) -> Result<()>,
) -> anyhow::Error {
    if !matches!(
        reopen_refusal_gate(&error),
        Some(REOPEN_CONFLICT_GATE | REOPEN_VERIFICATION_FAILED_GATE)
    ) {
        return error;
    }
    match close(&error) {
        Ok(()) => error.context(format!(
            "the reopened session {id} was closed before any prompt was delivered"
        )),
        Err(close_error) => error.context(format!(
            "the reopened session {id} could not be closed and may still hold the conversation: {close_error:#}"
        )),
    }
}

// What the read-only gates established about a closed source session.
#[derive(Debug)]
struct ReopenSource {
    manifest: SessionManifest,
    provider: FirstPartyCli,
    provider_session_id: String,
    event_id: String,
}

pub(super) fn run_reopen(request: ReopenRequest) -> Result<()> {
    let source = request.id.clone();
    let mut report = TurnReport::new(
        request.json,
        serde_json::Map::from_iter([("source_session".into(), serde_json::json!(source))]),
    );
    let outcome = run_reopen_inner(request, &mut report);
    match report.address() {
        Some((session, _)) => {
            let (outcome, gate) =
                settle_reopen_outcome(Reader::session_directory, &source, session, outcome);
            let mut extra = serde_json::Map::new();
            extra.insert("source_session".to_owned(), serde_json::json!(source));
            extra.insert("gate".to_owned(), serde_json::json!(gate));
            report.finish(outcome, extra)
        }
        None => {
            let extra = serde_json::json!({
                "session": null,
                "request_id": null,
                "gate": outcome.as_ref().err().and_then(reopen_refusal_gate),
            })
            .as_object()
            .unwrap()
            .clone();
            report.finish(outcome, extra)
        }
    }
}

// Only a launch-phase refusal can release the source marker. A later tell can fail while
// the initial messenger settles; its refusal cannot prove the initial prompt was not sent.
fn settle_reopen_outcome(
    session_directory: impl Fn(&str) -> Result<PathBuf>,
    source: &str,
    session: &str,
    outcome: Result<()>,
) -> (Result<()>, Option<String>) {
    let gate = outcome.as_ref().err().and_then(|error| {
        reopen_refusal_gate(error).map(str::to_owned).or_else(|| {
            session_directory(session)
                .ok()
                .and_then(|directory| read_reopen_refusal_gate(&directory))
        })
    });
    let outcome = match gate.as_deref() {
        Some(gate) if REOPEN_POST_CREATION_GATES.contains(&gate) => {
            match (session_directory(source), session_directory(session)) {
                (Ok(source_directory), Ok(refused_directory)) => {
                    release_reopen_marker_after_refusal(
                        &source_directory,
                        &refused_directory,
                        gate,
                        outcome,
                    )
                }
                (Err(error), _) | (_, Err(error)) => outcome.context(format!(
                    "the reopen marker of source session {source} was not released: {error:#}"
                )),
            }
        }
        _ => outcome,
    };
    (outcome, gate)
}

fn run_reopen_inner(request: ReopenRequest, report: &mut TurnReport) -> Result<()> {
    let options = session_start::StartOptions {
        timeout: request.timeout,
        detach: request.detach,
        json: request.json,
    };
    session_start::start(options, &mut ReopenedSource::new(request), report)
}

struct ReopenedSource {
    request: ReopenRequest,
    resolved: Option<(PathBuf, ReopenSource)>,
    resumed_from: Option<ResumedFrom>,
}

impl ReopenedSource {
    fn new(request: ReopenRequest) -> Self {
        Self {
            request,
            resolved: None,
            resumed_from: None,
        }
    }
}

impl session_start::StartSource for ReopenedSource {
    fn resolve(&mut self, deadline: Instant) -> Result<session_start::Resolved> {
        let request = &self.request;
        let terminal_kind = terminal::select(request.terminal)?;
        let source_directory = Reader::session_directory(&request.id)?;
        // Every check against the source is read-only. Pending-completion recovery and dead-owner
        // repair are never run on it: a closed session has nothing to converge, and reopen must not
        // alter the record it continues from.
        let source = inspect_reopen_source(&source_directory, &request.id)?;
        provider::reopen(source.provider)
            .and_then(|support| support.verify_available(&source.provider_session_id))
            .map_err(|error| reopen_refusal("provider-unsupported", format!("{error:#}")))?;
        let workspace = source.manifest.workspace.canonicalize().with_context(|| {
            format!(
                "source workspace does not exist or cannot be resolved: {}",
                source.manifest.workspace.display()
            )
        })?;
        if !workspace.is_dir() {
            bail!(
                "source workspace is not a directory: {}",
                workspace.display()
            );
        }
        // A pre-create gate: the source marker is claimed only by create, and no refusal
        // record is written.
        Reader::state_root()
            .and_then(|root| session_start::admission::check(&root, &workspace))
            .map_err(|error| reopen_refusal(REOPEN_WORKSPACE_REFUSED_GATE, format!("{error:#}")))?;
        let provider_path = resolve_provider(source.provider)?;
        let provider_version =
            check_provider_version_until(source.provider, &provider_path, Some(deadline))?;
        let requested_title = request.title.clone().unwrap_or_else(|| {
            let workspace_name = workspace
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            format!(
                "{} · {workspace_name} (reopened {})",
                source.provider.as_str(),
                request.id
            )
        });
        let title = sanitize_title(&requested_title)?;
        let spec = SessionSpec {
            provider: source.provider,
            provider_path,
            provider_version,
            workspace,
            title,
            model: request.model.clone(),
            effort: request.effort.clone(),
            yolo: request.yolo,
            prompt: request.prompt.clone(),
        };
        self.resolved = Some((source_directory, source));
        Ok(session_start::Resolved {
            spec,
            terminal_kind,
            context_sources: Vec::new(),
            assess_consent: None,
        })
    }

    fn create(&mut self, mut spec: SessionSpec) -> Result<CreatedSession> {
        let (directory, source) = self.resolved.as_ref().expect("resolve precedes create");
        let (created, resumed_from) =
            create_reopened_session(directory, &self.request.id, source, || {
                spec.prompt = native_delegation_prompt(&delegation_source(), &spec.prompt);
                create_session(spec)
            })?;
        self.resumed_from = Some(resumed_from);
        Ok(created)
    }

    fn gate(&self, directory: &Path, gate: delivery::Gate, deadline: Instant) -> Result<()> {
        let (_, source) = self.resolved.as_ref().expect("resolve precedes gate");
        let resumed_from = self.resumed_from.as_ref().expect("create precedes gate");
        verify_reopened_conversation_exclusive(
            source.provider,
            directory,
            Some(resumed_from),
            deadline,
            match gate {
                delivery::Gate::AfterLaunch => ResumedHolderCheck::AfterLaunch,
                delivery::Gate::BeforeDelivery => ResumedHolderCheck::BeforeInitialDelivery,
            },
        )
    }

    fn delivery_failed(
        &self,
        directory: &Path,
        id: &str,
        error: anyhow::Error,
        deadline: Instant,
    ) -> anyhow::Error {
        close_surface_after_reopen_verification_failure(directory, id, error, deadline)
    }

    fn result_extra(&self) -> Result<session_start::ResultExtra> {
        let resumed_from = self
            .resumed_from
            .as_ref()
            .expect("create precedes result_extra");
        let mut result_extra = serde_json::Map::new();
        result_extra.insert(
            "source_session".to_owned(),
            serde_json::Value::String(self.request.id.clone()),
        );
        result_extra.insert(
            "resumed_from".to_owned(),
            serde_json::to_value(resumed_from)?,
        );
        Ok(result_extra)
    }
}

fn create_reopened_session(
    source_directory: &Path,
    source_id: &str,
    source: &ReopenSource,
    create: impl FnOnce() -> Result<CreatedSession>,
) -> Result<(CreatedSession, ResumedFrom)> {
    let marker = claim_reopen_marker(source_directory, source_id, &source.provider_session_id)?;
    let created = create()?;
    let resumed_from = ResumedFrom {
        session: source_id.to_owned(),
        provider_session_id: source.provider_session_id.clone(),
        event_id: source.event_id.clone(),
    };
    if let Err(error) = record_resumed_from(&created.directory, &created.manifest, &resumed_from)
        .and_then(|()| marker.finalize(&created.id))
    {
        let _ = update_status(
            &created.directory,
            SessionState::Failed,
            None,
            Some(format!("{error:#}")),
        );
        return Err(error).with_context(|| {
            format!(
                "failed to record reopen provenance for session {}",
                created.id
            )
        });
    }
    Ok((created, resumed_from))
}

// Release only the marker naming this refused launch, once its provider is verified gone.
// Keep resumed_from; pending cleanup retains the marker for the next reopen to reconcile.
fn release_reopen_marker_after_refusal(
    source_directory: &Path,
    refused_directory: &Path,
    gate: &str,
    outcome: Result<()>,
) -> Result<()> {
    release_reopen_marker_after_refusal_with(source_directory, refused_directory, outcome, || {
        refused_launch_cleanup(refused_directory, gate)
    })
}

fn release_reopen_marker_after_refusal_with(
    source_directory: &Path,
    refused_directory: &Path,
    outcome: Result<()>,
    cleanup: impl FnOnce() -> Result<RefusedLaunchCleanup>,
) -> Result<()> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let released = (|| -> Result<()> {
        let cleanup = cleanup()?;
        if let RefusedLaunchCleanup::Pending(reason) = &cleanup {
            let note = record_reopen_refusal_cleanup_pending(refused_directory, reason)
                .err()
                .map_or(String::new(), |error| {
                    format!("; the refusal record could not be annotated: {error:#}")
                });
            bail!("{cleanup}{note}");
        }
        let marker_path = Reader::open_unchecked(source_directory)
            .record(CoreRecord::ReopenMarker)
            .path()
            .to_owned();
        let _lock = Store::open_unchecked(source_directory).lock()?;
        let Some(text) = session::RecordReader::at(&marker_path).text()? else {
            return Ok(());
        };
        let marker: ReopenMarker = serde_json::from_str(&text).context("invalid reopen marker")?;
        if marker.reopened_by.as_deref() != Some(refused_session) {
            bail!(
                "reopen marker names {:?}, not the refused session {refused_session}",
                marker.reopened_by
            );
        }
        RecordStore::at(&marker_path).remove()
    })();
    let Err(release_error) = released else {
        return outcome;
    };
    let release_error = release_error.context(format!(
        "the reopen marker of source session {} was not released",
        source_directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    ));
    match outcome {
        Ok(()) => Err(release_error),
        Err(error) => Err(error.context(format!("{release_error:#}"))),
    }
}

// Require a tombstoned, settled source with resolved receipts and a provider identity.
// Validate receipts first so corrupt events consistently refuse as request-unresolved.
fn inspect_reopen_source(directory: &Path, id: &str) -> Result<ReopenSource> {
    let manifest = Reader::open_unchecked(directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    verify_reopen_source_is_closed(directory, id)?;
    // Reject linked or non-directory events paths before reads can escape the session.
    // An absent directory reaches the receipt/identity gates, which report what is missing.
    Reader::open_unchecked(directory)
        .events_directory_state()
        .map_err(|error| {
            reopen_refusal(
                "request-unresolved",
                format!("the recorded results of session {id} cannot be read: {error:#}"),
            )
        })?;
    let index = requests::list(&Reader::open_unchecked(directory))?;
    if index.unreadable > 0 {
        return Err(reopen_refusal(
            "request-unresolved",
            format!(
                "session {id} has {} unreadable request record(s); their delivery outcome cannot be verified",
                index.unreadable
            ),
        ));
    }
    // Every receipt must resolve to a readable, well-formed event of this provider. A
    // receipt whose event is missing, empty, malformed, or from another provider cannot
    // prove what that request delivered, whatever the latest event says.
    for receipt in &index.receipts {
        let event_path = Reader::open_unchecked(directory)
            .event(&receipt.event_file)
            .path()
            .to_owned();
        if !RecordReader::at(&event_path).is_regular_file()? {
            return Err(reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} has no recorded result; its delivery outcome is uncertain",
                    receipt.request_id
                ),
            ));
        }
        let event = session::RecordReader::at(&event_path).json::<SessionEvent>().map_err(|error| {
            reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} points at recorded result {} that cannot be read: {error:#}",
                    receipt.request_id, receipt.event_file
                ),
            )
        })?;
        if event.provider != provider.as_str() {
            return Err(reopen_refusal(
                "request-unresolved",
                format!(
                    "request {} of session {id} points at recorded result {} of provider {} instead of {}",
                    receipt.request_id,
                    receipt.event_file,
                    event.provider,
                    provider.as_str()
                ),
            ));
        }
    }
    let (event_id, provider_session_id) =
        latest_provider_event_identity(directory, provider, id)?.ok_or_else(|| {
            reopen_refusal(
                "source-identity-missing",
                format!(
                    "session {id} has no {} event that records a provider session id; a session whose only turn failed cannot be reopened",
                    provider.as_str()
                ),
            )
        })?;
    Ok(ReopenSource {
        manifest,
        provider,
        provider_session_id,
        event_id,
    })
}

// A refused launch with verified cleanup no longer holds the conversation; the next
// claim removes its stale marker under the source lock before installing its own.
#[derive(Debug)]
struct StaleReopenMarker {
    refused_session: String,
    gate: String,
    cleanup: RefusedLaunchCleanup,
}

// Read-only: release eligibility requires a durable launch refusal and verified cleanup.
// Missing records or a possibly surviving provider keep the marker consumed after a crash.
fn verify_reopen_source_is_closed(directory: &Path, id: &str) -> Result<Option<StaleReopenMarker>> {
    let closed = Reader::open_unchecked(directory).regular_closed_if_present()?;
    let status = Reader::open_unchecked(directory).regular_status_if_present()?;
    let state = status
        .as_ref()
        .map_or("unknown", |status| status.state.as_str());
    if state != SessionState::Closed.as_str()
        || closed
            .as_ref()
            .is_none_or(|closed| closed.state != SessionState::Closed)
    {
        return Err(reopen_refusal(
            "source-not-closed",
            format!(
                "session {id} is {state}; reopen requires a session closed with its closed tombstone"
            ),
        ));
    }
    for name in [
        CoreRecord::TurnClaim.name(),
        CoreRecord::Completion.name(),
        CoreRecord::Terminal.name(),
        CoreRecord::TerminalClosing.name(),
    ] {
        if fs::symlink_metadata(Reader::open_unchecked(directory).private(name).path()).is_ok() {
            return Err(reopen_refusal(
                "source-not-converged",
                format!("session {id} still carries {name}; its close has not converged"),
            ));
        }
    }
    let Some(text) = Reader::open_unchecked(directory)
        .record(CoreRecord::ReopenMarker)
        .text()?
    else {
        return Ok(None);
    };
    let refuse = |detail: String| Err(reopen_refusal("already-reopened", detail));
    let Ok(marker) = serde_json::from_str::<ReopenMarker>(&text) else {
        return refuse(format!(
            "session {id} carries a reopen marker that cannot be read; it is treated as consumed"
        ));
    };
    let Some(new_id) = marker.reopened_by else {
        return refuse(format!("a reopen of session {id} is already in progress"));
    };
    let already = format!("session {id} was already reopened as {new_id}");
    if !valid_session_id(&new_id) {
        return refuse(format!(
            "{already}; the marker names an invalid session id, so it is treated as consumed"
        ));
    }
    let refused_directory = directory
        .parent()
        .context("session directory has no state root")?
        .join(&new_id);
    if !fs::symlink_metadata(&refused_directory).is_ok_and(|metadata| metadata.is_dir()) {
        return refuse(format!(
            "{already}; the records of {new_id} are missing, so the marker is treated as consumed"
        ));
    }
    let Some(refusal) = read_reopen_launch_refusal(&refused_directory) else {
        return refuse(already);
    };
    let cleanup = match refused_launch_cleanup(&refused_directory, &refusal.gate) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            return refuse(format!(
                "{already}; that launch was refused ({}) but its records cannot be verified: {error:#}",
                refusal.gate
            ));
        }
    };
    if !cleanup.releases_marker() {
        return refuse(format!(
            "{already}; that launch was refused ({}) but {cleanup}",
            refusal.gate
        ));
    }
    Ok(Some(StaleReopenMarker {
        refused_session: new_id,
        gate: refusal.gate,
        cleanup,
    }))
}

// Read every event, including legacy and older events: any unreadable outcome refuses
// as request-unresolved even without a receipt pointing at it.
fn latest_provider_event_identity(
    directory: &Path,
    provider: FirstPartyCli,
    id: &str,
) -> Result<Option<(String, String)>> {
    let mut newest = None;
    for path in Reader::open_unchecked(directory).events()? {
        let event: SessionEvent = session::RecordReader::at(&path).json().map_err(|error| {
            reopen_refusal(
                "request-unresolved",
                format!(
                    "session {id} has a recorded result {} that cannot be read: {error:#}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                ),
            )
        })?;
        if event.provider != provider.as_str() {
            continue;
        }
        if let Some(provider_session_id) = event.provider_session_id
            && !provider_session_id.trim().is_empty()
        {
            let event_id = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("event path has no file name")?
                .to_owned();
            newest = Some((event_id, provider_session_id));
        }
    }
    Ok(newest)
}

pub(super) fn read_resumed_from(directory: &Path) -> Result<Option<ResumedFrom>> {
    let provenance: ReopenProvenance = Reader::open_unchecked(directory)
        .record(CoreRecord::Manifest)
        .json()?;
    Ok(provenance.resumed_from)
}

fn record_resumed_from(
    directory: &Path,
    manifest: &SessionManifest,
    resumed_from: &ResumedFrom,
) -> Result<()> {
    let mut value = serde_json::to_value(manifest)?;
    value
        .as_object_mut()
        .context("session manifest is not a JSON object")?
        .insert(
            "resumed_from".to_owned(),
            serde_json::to_value(resumed_from)?,
        );
    Store::open_unchecked(directory)
        .record(CoreRecord::Manifest)
        .write_json(&value)
}

// The winner's hold on a closed source. Dropping it before `finalize` removes the marker
// again, so a reopen that never created its session leaves the source reopenable.
#[derive(Debug)]
struct ReopenMarkerClaim {
    path: PathBuf,
    claim: String,
    finalized: bool,
}

impl ReopenMarkerClaim {
    fn finalize(mut self, new_session_id: &str) -> Result<()> {
        let _lock = Store::open_unchecked((self.path).with_file_name("")).lock()?;
        let text = session::RecordReader::at(&self.path)
            .text()?
            .context("reopen marker disappeared before the new session was recorded")?;
        let mut marker: ReopenMarker =
            serde_json::from_str(&text).context("invalid reopen marker")?;
        if marker.claim != self.claim {
            bail!("reopen marker belongs to a different reopen attempt");
        }
        marker.reopened_by = Some(new_session_id.to_owned());
        session::RecordStore::at(&self.path).write_json(&marker)?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for ReopenMarkerClaim {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let Ok(_lock) = Store::open_unchecked((self.path).with_file_name("")).lock() else {
            return;
        };
        let current = session::RecordReader::at(&self.path)
            .text()
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<ReopenMarker>(&text).ok());
        if current.is_some_and(|marker| marker.claim == self.claim) {
            let _ = RecordStore::at(&self.path).remove();
        }
    }
}

// Serializes concurrent reopens of one closed source under the source's own turn-claim lock:
// the closed gates are re-checked under the lock and the marker is created with
// `create_new`, so exactly one attempt can hold it.
fn claim_reopen_marker(
    directory: &Path,
    id: &str,
    provider_session_id: &str,
) -> Result<ReopenMarkerClaim> {
    let path = Reader::open_unchecked(directory)
        .record(CoreRecord::ReopenMarker)
        .path()
        .to_owned();
    let _lock = Store::open_unchecked(directory).lock()?;
    if let Some(stale) = verify_reopen_source_is_closed(directory, id)? {
        // The gate re-ran under the lock, so the stale marker still names a refused launch
        // whose cleanup is verified now; releasing it here is the reconciliation the
        // crashed or timed-out parent never performed.
        RecordStore::at(&path).remove().with_context(|| {
            format!(
                "could not release the stale reopen marker of session {id} (reopened as {}, refused at {}, {})",
                stale.refused_session, stale.gate, stale.cleanup
            )
        })?;
    }
    let claim = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        TURN_CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let marker = ReopenMarker {
        schema: 1,
        claim: claim.clone(),
        provider_session_id: provider_session_id.to_owned(),
        reopened_by: None,
        created_unix_ms: unix_ms(),
    };
    session::RecordStore::at(&path)
        .write_private(&serde_json::to_vec_pretty(&marker)?)
        .map_err(|error| {
            reopen_refusal(
                "already-reopened",
                format!("could not claim session {id} for reopen: {error:#}"),
            )
        })?;
    Ok(ReopenMarkerClaim {
        path,
        claim,
        finalized: false,
    })
}

// Recheck before every follow-up; refusal atomically releases the claim and records the
// reason, leaving its receipt unresolved. A later foreign resume waits for the next check.
#[allow(clippy::too_many_arguments)]
pub(super) fn refuse_follow_up_to_shared_conversation(
    provider: FirstPartyCli,
    directory: &Path,
    id: &str,
    resumed_from: Option<&ResumedFrom>,
    deadline: Instant,
    claim: &mut turn::Claim,
    previous_state: &SessionState,
) -> Result<()> {
    let Err(error) = verify_reopened_conversation_exclusive(
        provider,
        directory,
        resumed_from,
        deadline,
        ResumedHolderCheck::BeforeFollowUp,
    ) else {
        return Ok(());
    };
    let error = error.context(format!(
        "follow-up to reopened session {id} was refused before delivery"
    ));
    Err(record_follow_up_refusal(id, claim, previous_state, error))
}

// Release and status update share the lifecycle lock and require this request's claim.
// A newer turn keeps its claim, receipt and status; the refused receipt stays unresolved.
fn record_follow_up_refusal(
    id: &str,
    claim: &mut turn::Claim,
    previous_state: &SessionState,
    error: anyhow::Error,
) -> anyhow::Error {
    match claim.release_now_with_reason(previous_state.clone(), format!("{error:#}")) {
        Ok(true) => error,
        Ok(false) => error.context(format!(
            "the turn claim of session {id} already belonged to another request; its status was left unchanged"
        )),
        Err(release_error) => error.context(format!(
            "the turn claim of session {id} could not be released: {release_error:#}"
        )),
    }
}

// Doctor observes the same marker eligibility as the locked reopen gate, using the
// provider identity; it never releases or annotates the marker.
pub(super) fn marker_check(reader: &Reader, id: &str, checks: &mut Vec<Check>) {
    let directory = reader.directory();
    use Availability::*;
    let marker = match RecordReader::at(
        Reader::open_unchecked(directory)
            .record(CoreRecord::ReopenMarker)
            .path(),
    )
    .optional_json::<ReopenMarker>()
    {
        Ok(Some(marker)) => marker,
        Ok(None) => return,
        Err(error) => {
            checks.push(Check::new("reopen_marker", Unknown, "reopen_marker_unreadable", format!("{error:#}"), "reopen treats an unreadable marker as consumed; inspect the marker before any reopen."));
            return;
        }
    };
    let next_action = "reopen releases a marker only when the session it names recorded a launch refusal and either no provider process was spawned or the provider process recorded in its provider-process.json is verified gone (pid dead, or alive under another identity); doctor never releases or repairs it.";
    let Some(reopened_by) = marker.reopened_by else {
        checks.push(Check::new("reopen_marker", Unavailable, "reopen_in_progress", format!("A reopen of {id} holds the marker and has not recorded its new session yet; a new reopen is refused with gate already-reopened."), next_action)
            .evidence(json!({"claim": marker.claim})));
        return;
    };
    let refused_directory = directory.parent().map(|root| root.join(&reopened_by));
    let refusal = match &refused_directory {
        Some(refused_directory)
            if valid_session_id(&reopened_by)
                && fs::symlink_metadata(refused_directory)
                    .is_ok_and(|metadata| metadata.is_dir()) =>
        {
            read_reopen_launch_refusal(refused_directory)
        }
        _ => None,
    };
    let (availability, reason, detail, cleanup) = match refusal
        .as_ref()
        .zip(refused_directory.as_deref())
    {
        None => (
            Unavailable,
            "reopen_marker_consumed",
            format!(
                "Session {id} was reopened as {reopened_by}, which recorded no launch refusal; a new reopen is refused with gate already-reopened."
            ),
            Value::Null,
        ),
        Some((refusal, refused_directory)) => {
            match refused_launch_cleanup(refused_directory, &refusal.gate) {
                Ok(cleanup) if cleanup.releases_marker() => (
                    Available,
                    "reopen_marker_reconcilable",
                    format!(
                        "The reopen as {reopened_by} was refused at launch (gate {}) and {cleanup}; the next reopen of {id} releases the marker and proceeds.",
                        refusal.gate
                    ),
                    json!(cleanup.to_string()),
                ),
                Ok(cleanup) => (
                    Unavailable,
                    "reopen_marker_retained",
                    format!(
                        "The reopen as {reopened_by} was refused at launch (gate {}) but {cleanup}; the marker stays consumed and a new reopen is refused with gate already-reopened until the provider process of {reopened_by} is verified gone. Closing its console or the exit of its launch wrapper is not that evidence on its own.",
                        refusal.gate
                    ),
                    json!(cleanup.to_string()),
                ),
                Err(error) => (
                    Unknown,
                    "reopen_marker_unverified",
                    format!(
                        "The reopen as {reopened_by} was refused at launch (gate {}) but its records cannot be verified: {error:#}",
                        refusal.gate
                    ),
                    Value::Null,
                ),
            }
        }
    };
    checks.push(Check::new("reopen_marker", availability, reason, detail, next_action).evidence(json!({
        "reopened_by": reopened_by,
        "gate": refusal.as_ref().map(|refusal| &refusal.gate),
        "recorded_cleanup": refusal.as_ref().and_then(|refusal| refusal.cleanup.as_ref()),
        "recorded_cleanup_detail": refusal.as_ref().and_then(|refusal| refusal.cleanup_detail.as_ref()),
        "cleanup": cleanup,
    })));
}

#[cfg(test)]
pub(super) mod tests;
