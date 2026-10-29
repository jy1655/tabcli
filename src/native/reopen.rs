//! Reopen of a closed Session: source reservation, launch refusal, and cleanup settlement.
//! Provider adapters retain conversation-holder evidence; terminal ownership retains Close authority.
use super::args::{
    CommonStartOptions, OptionSet, ValidatedStartOptions, parse_common_start_option,
    validate_start_options,
};
use super::close::close_session_surface;
use crate::native::session::primitives::require_valid_session_id;
use crate::native::session::schema::{SessionEvent, SessionManifest};
use crate::native::session::{
    CoreRecord, Reader, RecordReader, SessionState, Store, record, requests, turn, update_status,
};
use crate::native::turn_report::TurnReport;
use crate::native::{
    CreatedSession, DEFAULT_TIMEOUT_SECS, NativeCommand, SessionSpec, consent, delivery, provider,
    session, session_start, terminal,
};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

mod gate;
mod reservation;
use gate::reopen_refusal;
pub(super) use gate::{ReopenGate, reopen_refusal_gate};
use reservation::{
    Reservation, record_failed_launch, retry_interrupted_launch_fence, settle_reopen_outcome,
    verify_reopen_source_is_closed,
};
pub(super) use reservation::{marker_check, record_reopen_refusal};

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
    let refuse = |gate: ReopenGate, detail: String| {
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
                ReopenGate::ReopenVerificationFailed,
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
        ReopenGate::ReopenConflict,
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
        Some(ReopenGate::ReopenConflict | ReopenGate::ReopenVerificationFailed)
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
    let mut report = TurnReport::new(
        request.json,
        serde_json::Map::from_iter([("source_session".into(), serde_json::json!(request.id))]),
    );
    let (outcome, gate) = run_reopen_inner(request, &mut report);
    let mut extra = serde_json::Map::from_iter([("gate".into(), serde_json::json!(gate))]);
    if report.address().is_none() {
        // A failure before any Request address still shows where the address would be.
        extra.insert("session".into(), serde_json::Value::Null);
        extra.insert("request_id".into(), serde_json::Value::Null);
    }
    report.finish(outcome, extra)
}

fn run_reopen_inner(
    request: ReopenRequest,
    report: &mut TurnReport,
) -> (Result<()>, Option<ReopenGate>) {
    let prepared = (|| {
        let start = session_start::Start::new(session_start::StartOptions {
            timeout: request.timeout,
            detach: request.detach,
            json: request.json,
        })?;
        let attempt = ResolvedReopen::resolve(request, start.deadline())?.create()?;
        Ok::<_, anyhow::Error>((start, attempt))
    })();
    match prepared {
        Ok((start, attempt)) => attempt.run(&start, report),
        Err(error) => {
            let gate = reopen_refusal_gate(&error);
            (Err(error), gate)
        }
    }
}

struct ResolvedReopen {
    source_id: String,
    source_directory: PathBuf,
    source: ReopenSource,
    start: session_start::Resolved,
}

// A successfully created Reopen owns its source reservation through every later phase.
// Its run always settles after the shared Initial prompt operation and its Claim drop,
// independently of whether a Request address could be recorded.
struct ReopenAttempt {
    source_id: String,
    source_directory: PathBuf,
    resumed_from: ResumedFrom,
    prepared: session_start::Prepared,
}

impl ReopenAttempt {
    fn run(
        self,
        start: &session_start::Start,
        report: &mut TurnReport,
    ) -> (Result<()>, Option<ReopenGate>) {
        let outcome = start.run(&self.prepared, &self, report);
        settle_reopen_outcome(
            &self.source_directory,
            &self.prepared.created.directory,
            outcome,
        )
    }
}

// What reopen resolves differently from a fresh start. The provider and the workspace are
// the source's: its recorded, canonical path, so the shared resolution assesses consent for
// the canonical path (reopen takes no path alias). That assessment is the new session's own,
// from the current stores and exactly as a fresh `ask` makes it, so that it can take the same
// guarded trust-dialog answer; the source's assessment is diagnostic history of another
// session, never consent for this one (#202). The default title names the source, the two
// workspace errors say "source workspace", and an admission refusal is typed as the
// workspace-refused gate: a pre-create gate, as the source marker is claimed only by create
// and no refusal record is written. Model, effort and yolo come from the request alone.
pub(super) fn resolution<'a>(
    request: &'a ReopenRequest,
    provider: FirstPartyCli,
    workspace: &'a Path,
) -> session_start::Resolution<'a> {
    session_start::Resolution {
        provider,
        workspace,
        consent_path: session_start::ConsentPath::Canonical,
        title: request.title.clone(),
        default_title: session_start::DefaultTitle::Reopened {
            source: &request.id,
        },
        model: request.model.clone(),
        effort: request.effort.clone(),
        yolo: request.yolo,
        prompt: request.prompt.clone(),
        terminal: request.terminal,
        context_sources: Vec::new(),
        workspace_noun: "source workspace",
        admission_refusal: |error| {
            reopen_refusal(ReopenGate::WorkspaceRefused, format!("{error:#}"))
        },
    }
}

impl ResolvedReopen {
    fn resolve(request: ReopenRequest, deadline: Instant) -> Result<Self> {
        let source_directory = Reader::session_directory(&request.id)?;
        let _ = retry_interrupted_launch_fence(&source_directory);
        // Every check against the source is read-only. Pending-completion recovery and dead-owner
        // repair are never run on it: a closed session has nothing to converge, and reopen must not
        // alter the record it continues from.
        let source = inspect_reopen_source(&source_directory, &request.id)?;
        provider::reopen(source.provider)
            .and_then(|support| support.verify_available(&source.provider_session_id))
            .map_err(|error| {
                reopen_refusal(ReopenGate::ProviderUnsupported, format!("{error:#}"))
            })?;
        // The shared resolution fails before anything is created, so a failed consent
        // assessment leaves the source reopenable.
        let start = session_start::resolve(
            resolution(&request, source.provider, &source.manifest.workspace),
            deadline,
        )?;
        Ok(Self {
            source_id: request.id,
            source_directory,
            source,
            start,
        })
    }

    fn create(self) -> Result<ReopenAttempt> {
        self.create_with(session_start::create_with_consent)
    }

    fn create_with(
        self,
        create: impl FnOnce(SessionSpec, Option<&consent::Assessment>) -> Result<CreatedSession>,
    ) -> Result<ReopenAttempt> {
        let (prepared, resumed_from) = self.start.create_with(|spec, consent| {
            create_reopened_session(
                &self.source_directory,
                &self.source_id,
                &self.source,
                || create(spec, consent),
            )
        })?;
        Ok(ReopenAttempt {
            source_id: self.source_id,
            source_directory: self.source_directory,
            resumed_from,
            prepared,
        })
    }
}

impl session_start::InitialSource for ReopenAttempt {
    fn result_extra(&self) -> Result<session_start::ResultExtra> {
        let mut result_extra = serde_json::Map::new();
        result_extra.insert(
            "source_session".to_owned(),
            serde_json::Value::String(self.source_id.clone()),
        );
        result_extra.insert(
            "resumed_from".to_owned(),
            serde_json::to_value(&self.resumed_from)?,
        );
        Ok(result_extra)
    }

    fn gate(&self, directory: &Path, gate: delivery::Gate, deadline: Instant) -> Result<()> {
        verify_reopened_conversation_exclusive(
            self.prepared.provider,
            directory,
            Some(&self.resumed_from),
            deadline,
            match gate {
                delivery::Gate::AfterLaunch => ResumedHolderCheck::AfterLaunch,
                delivery::Gate::BeforeDelivery => ResumedHolderCheck::BeforeInitialDelivery,
            },
        )
    }

    fn failed(
        &self,
        created: &CreatedSession,
        failure: session_start::Failure<'_>,
    ) -> anyhow::Error {
        match failure {
            session_start::Failure::Preparing(error) => {
                // Record before status: an interrupted attempt is reconcilable by the
                // next Reopen. Marker release belongs to run's one settlement path.
                let refusal = record_reopen_refusal(
                    &created.directory,
                    ReopenGate::StartFailed,
                    format!("{error:#}"),
                );
                session_start::mark_start_failed(created, &refusal);
                refusal
            }
            session_start::Failure::Launch { claim, error } => {
                fail_reopen_launch(created, claim, error)
            }
            session_start::Failure::Delivery { error, deadline } => {
                close_surface_after_reopen_verification_failure(
                    &created.directory,
                    &created.id,
                    error,
                    deadline,
                )
            }
        }
    }
}

fn fail_reopen_launch(
    created: &CreatedSession,
    initial_claim: &mut session::turn::Claim,
    error: anyhow::Error,
) -> anyhow::Error {
    initial_claim.fence_launch();
    let mut detail = format!("{error:#}");
    let gate = match record_failed_launch(&created.directory, initial_claim.token(), &error) {
        Ok(gate) => gate,
        Err(record_error) => {
            detail.push_str(&format!(
                "; the refusal record could not be written: {record_error:#}"
            ));
            reopen_refusal_gate(&error).unwrap_or(ReopenGate::LaunchFailed)
        }
    };
    if let Err(fence_error) = session::launch::fence_failed_start(
        &Store::open_unchecked(&created.directory),
        initial_claim.token(),
        &detail,
    ) {
        detail.push_str(&format!(
            "; the failed launch could not be fenced: {fence_error:#}"
        ));
    }
    reopen_refusal(gate, detail)
}

fn create_reopened_session(
    source_directory: &Path,
    source_id: &str,
    source: &ReopenSource,
    create: impl FnOnce() -> Result<CreatedSession>,
) -> Result<(CreatedSession, ResumedFrom)> {
    let marker = Reservation::claim(source_directory, source_id, &source.provider_session_id)?;
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
                ReopenGate::RequestUnresolved,
                format!("the recorded results of session {id} cannot be read: {error:#}"),
            )
        })?;
    let index = requests::list(&Reader::open_unchecked(directory))?;
    if index.unreadable > 0 {
        return Err(reopen_refusal(
            ReopenGate::RequestUnresolved,
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
                ReopenGate::RequestUnresolved,
                format!(
                    "request {} of session {id} has no recorded result; its delivery outcome is uncertain",
                    receipt.request_id
                ),
            ));
        }
        let event = session::RecordReader::at(&event_path).json::<SessionEvent>().map_err(|error| {
            reopen_refusal(
                ReopenGate::RequestUnresolved,
                format!(
                    "request {} of session {id} points at recorded result {} that cannot be read: {error:#}",
                    receipt.request_id, receipt.event_file
                ),
            )
        })?;
        if event.provider != provider.as_str() {
            return Err(reopen_refusal(
                ReopenGate::RequestUnresolved,
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
                ReopenGate::SourceIdentityMissing,
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
                ReopenGate::RequestUnresolved,
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
    // The manifest's provenance projection: the same regular-file, strict read as the
    // manifest itself, judged by this projection's shape alone.
    let path = Reader::open_unchecked(directory)
        .record(CoreRecord::Manifest)
        .path()
        .to_owned();
    let provenance: ReopenProvenance =
        record::decode(&session::primitives::read_regular_bytes(&path)?)
            .map_err(|error| error.in_path(&path))?;
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

#[cfg(test)]
pub(super) use reservation::tests;

// Continues a closed session's provider conversation in a new session. Model, effort, and
// yolo are never inherited from the source manifest; only the options stated here apply.
#[derive(Debug)]
pub(crate) struct ReopenRequest {
    pub(crate) id: String,
    pub(crate) prompt: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) terminal: Option<terminal::TerminalKind>,
    pub(crate) yolo: bool,
    pub(crate) timeout: Duration,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

pub(super) fn parse_reopen(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("reopen requires one closed session id")?;
    require_valid_session_id(id)?;
    let mut parsed = CommonStartOptions::default();
    let mut index = 0;
    while index < options.len() {
        if parse_common_start_option(options, &mut index, &mut parsed, OptionSet::Start)? {
            continue;
        }
        bail!("unknown reopen option: {}", options[index]);
    }
    let ValidatedStartOptions {
        prompt,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout,
        detach,
        json,
    } = validate_start_options(parsed, "reopen")?;
    Ok(NativeCommand::Reopen(ReopenRequest {
        id: id.to_owned(),
        prompt,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}
