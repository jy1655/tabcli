//! Reopen of a closed Session: source reservation, launch refusal, and cleanup settlement.
//! Provider adapters retain conversation-holder evidence; terminal ownership retains Close authority.
use super::doctor::{Availability, Check};
use super::*;
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

// The gates a reopen can fail after its session exists. Each one leaves the new session
// failed with no prompt delivered. The source's reopen marker is released again only once
// the refused launch provably cannot hold the conversation (`RefusedLaunchCleanup`).
const REOPEN_POST_CREATION_GATES: [&str; 3] = [
    REOPEN_LAUNCH_GATE,
    REOPEN_CONFLICT_GATE,
    REOPEN_VERIFICATION_FAILED_GATE,
];

// The only phase whose refusals are persisted. The record exists so the reopen command can
// name a gate that failed in the launch wrapper, in another process; every writer of it is
// a launch-phase gate (the pre-spawn recheck, the post-launch holder check, and the holder
// check immediately before the initial prompt is sent), and the reader accepts nothing
// else. A refused later `tell` keeps its reason in the session status and its own response
// only: it must never be mistaken for a refusal of the initial delivery, whose uncertain
// outcome is decided from this record.
const REOPEN_REFUSAL_LAUNCH_PHASE: &str = "launch";

// `cleanup` is written by a marker settlement that found the refused launch may still hold
// the conversation (a failed close of a spawned process). It is a note for `doctor` and the
// next reopen attempt; the release decision itself is always taken from the session's live
// records, never from this field.
const REOPEN_REFUSAL_CLEANUP_PENDING: &str = "pending";

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct RecordedReopenRefusal {
    schema: u32,
    phase: String,
    gate: String,
    detail: String,
    created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_detail: Option<String>,
}

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
    IdentityMismatch(&'static str),
}

// What can be observed about a recorded provider process now.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum ProviderProcessObservation {
    Gone(ProviderProcessGone),
    // The pid is alive and, where an identity was recorded, still carries it.
    Alive,
    // The pid is alive (or its liveness cannot be denied) but its identity could not be
    // inspected, so neither survival nor reuse is established.
    Unknown(String),
}

// Reopen-local observation of a recorded provider process. Unlike the owner observation
// used by dead-owner repair (`query::observe_owner_record`), it keeps a confirmed identity
// mismatch apart from a failure to inspect: the first is a pid reused by another process
// and releases the marker, the second retains it.
fn observe_provider_process(record: &ProviderProcessRecord) -> ProviderProcessObservation {
    #[cfg(windows)]
    if let Some(identity) = &record.windows_process_identity {
        return classify_provider_process_identity(
            record.pid,
            terminal::check_windows_process_identity(record.pid, identity),
        );
    }
    if process_is_alive(record.pid) {
        ProviderProcessObservation::Alive
    } else {
        ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
    }
}

// Maps the typed Windows identity check onto the observation. An inspection failure is
// `Gone` only when the pid is not alive at all; a pid that is alive (or access-denied, which
// liveness treats as alive) but uninspectable stays `Unknown`.
#[cfg(any(windows, test))]
fn classify_provider_process_identity(
    pid: u32,
    check: Result<terminal::WindowsProcessIdentityCheck>,
) -> ProviderProcessObservation {
    match check {
        Ok(terminal::WindowsProcessIdentityCheck::Matches) => ProviderProcessObservation::Alive,
        Ok(terminal::WindowsProcessIdentityCheck::Mismatch(reason)) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::IdentityMismatch(reason))
        }
        Err(_) if !process_is_alive(pid) => {
            ProviderProcessObservation::Gone(ProviderProcessGone::Exited)
        }
        Err(error) => ProviderProcessObservation::Unknown(format!("{error:#}")),
    }
}

// Whether a launch-refused reopened session can still hold the provider conversation. The
// source's reopen marker is released only on the two verified outcomes; `Pending` keeps it
// consumed, because a refused launch whose provider process survived (a close that failed
// or was never performed, a process that ignored the console close, a process that never
// registered) is exactly the second live writer the gate exists to keep out, and the
// registry scan of the next reopen would not see an unregistered one. Neither the launch
// wrapper's liveness nor a closed surface is evidence on its own: Windows does not end a
// child with its parent, dead-owner repair marks a session closed without any terminal
// close, and a provider can outlive the console it was started in.
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

// Read-only. Establishes, from the refused session's own records, whether the launch that
// was refused under `gate` can still hold the conversation. A session that accepts prompts
// or is working is never a refused launch, whatever its record says. The pre-spawn gate
// proves nothing by itself: only the absence of a provider process record shows that no
// process was spawned, and a record that exists is verified like any other.
fn refused_launch_cleanup(refused_directory: &Path, gate: &str) -> Result<RefusedLaunchCleanup> {
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
    let evidence = match observe_provider_process(&record) {
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

// The holder check of a reopened session. The adapter reports every other live holder of the
// conversation; a non-empty answer is a detected conflict and refuses under
// `reopen-conflict`. Any failure to complete the check (unreadable registry, a live record
// that cannot be verified, an uninspectable process, a duplicate managed name, a registration
// that never came) refuses under `reopen-verification-failed`: an unverifiable conversation
// is treated as shared, never as exclusive. At the launch-phase boundaries both refusals are
// recorded in the session so the gate survives the process boundary; a follow-up refusal is
// returned unrecorded. The recorded detail states only what was detected; whether the new
// surface was then closed is reported by the caller once that outcome is known. A foreign
// resume that registers between two checks is not detected until the next one.
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
    let others = match provider::other_resumed_conversation_holders(
        provider,
        provider::ResumedSessionContext {
            directory,
            provider_session_id: &resumed_from.provider_session_id,
            deadline,
            wait_for_registration: check == ResumedHolderCheck::AfterLaunch,
        },
    ) {
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

// A failed holder check, whether a detected conflict or a verification the adapter could
// not complete, is the launch failure that also closes the new surface: the reopened
// process is a second live writer of the conversation (or cannot be shown not to be), and
// leaving it open would keep the interleaving the gate exists to detect. Every other launch
// failure keeps the existing behavior of marking only the new session failed. Only the new
// session's own handle is ever closed; the source keeps its tombstone. The returned error
// reports the close outcome only after it is known.
pub(super) fn close_surface_after_reopen_verification_failure(
    directory: &Path,
    id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    close_surface_after_reopen_verification_failure_with(id, error, |detected| {
        close_session_surface(directory, id, Some(format!("{detected:#}")))
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
    let json = request.json;
    let source = request.id.clone();
    let mut address = None;
    let outcome = run_reopen_inner(request, &mut address);
    match address {
        Some((session, request_id)) => {
            let (outcome, gate) =
                settle_reopen_outcome(Reader::session_directory, &source, &session, outcome);
            let mut extra = serde_json::Map::new();
            extra.insert("source_session".to_owned(), serde_json::json!(source));
            extra.insert("gate".to_owned(), serde_json::json!(gate));
            finish_request_with_extra(outcome, json, &session, &request_id, extra)
        }
        None => {
            if let Err(error) = &outcome
                && json
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "source_session": source,
                        "session": null,
                        "request_id": null,
                        "gate": reopen_refusal_gate(error),
                        "error": format!("{error:#}"),
                    }))?
                );
            }
            outcome
        }
    }
}

// Decides, once the reopened session exists, whether the reopen was refused at a
// post-creation gate and whether that refusal releases the source's reopen marker. The gate
// is typed in this process (the post-launch and pre-initial-delivery holder checks) or
// recorded by the launch wrapper in the new session (the pre-spawn recheck); either way the
// response names it. Only a launch-phase refusal counts: the initial delivery can complete
// while its messenger is still settling, a later `tell` can then be refused and the session
// closed, and the initial messenger can finally report only that delivery is uncertain.
// That `tell` refusal lives in the session status, never in the launch refusal record, so
// the uncertain outcome finds no gate here and the marker stays consumed: a prompt may
// have reached the conversation, and a second reopen must not be permitted on the strength
// of a refusal that was not the launch's.
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

fn run_reopen_inner(request: ReopenRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    let terminal_kind = terminal::select(request.terminal)?;
    let source_directory = Reader::session_directory(&request.id)?;
    // Every check against the source is read-only. Pending-completion recovery and dead-owner
    // repair are never run on it: a closed session has nothing to converge, and reopen must not
    // alter the record it continues from.
    let source = inspect_reopen_source(&source_directory, &request.id)?;
    provider::verify_reopen_available(source.provider, &source.provider_session_id)
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
    let provider_path = resolve_provider(source.provider)?;
    let provider_version =
        check_provider_version_until(source.provider, &provider_path, Some(deadline))?;
    let requested_title = request.title.unwrap_or_else(|| {
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
    let (created, resumed_from) =
        create_reopened_session(&source_directory, &request.id, &source, || {
            create_session(SessionSpec {
                provider: source.provider,
                provider_path,
                provider_version,
                workspace,
                title,
                model: request.model,
                effort: request.effort,
                yolo: request.yolo,
                prompt: native_delegation_prompt(&delegation_source(), &request.prompt),
            })
        })?;
    let mut result_extra = serde_json::Map::new();
    result_extra.insert(
        "source_session".to_owned(),
        serde_json::Value::String(request.id.clone()),
    );
    result_extra.insert(
        "resumed_from".to_owned(),
        serde_json::to_value(&resumed_from)?,
    );
    launch_created_session(
        SessionLaunch {
            created,
            provider: source.provider,
            terminal_kind,
            deadline,
            timeout: request.timeout,
            detach: request.detach,
            json: request.json,
            context_sources: &[],
            result_extra,
            resumed_from: Some(resumed_from),
        },
        address,
    )
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

// A reopen refused after its session existed delivered nothing to the conversation: the
// pre-spawn recheck started no process, and both post-launch gates refuse before the first
// prompt and close the new surface. Bridge therefore established no conversation writer,
// and the source's reopen marker is released so the source can be reopened again once the
// cause is gone. The refused session keeps its `resumed_from` as provenance. Release
// requires that the marker still names the refused session and that the refused launch
// provably cannot hold the conversation (`refused_launch_cleanup`): a spawned provider
// process may survive the console close and the wrapper without ever registering, so the
// registry gate of the next reopen would not catch it. Until that process is verified
// gone the marker stays consumed, the refusal record is annotated with `cleanup:
// "pending"`, and the next reopen attempt reconciles the marker
// (`verify_reopen_source_is_closed`).
fn release_reopen_marker_after_refusal(
    source_directory: &Path,
    refused_directory: &Path,
    gate: &str,
    outcome: Result<()>,
) -> Result<()> {
    let refused_session = refused_directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let released = (|| -> Result<()> {
        let cleanup = refused_launch_cleanup(refused_directory, gate)?;
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

// Read-only gates on the closed source: it is closed with its tombstone and nothing of its
// lifecycle is left open, every request record resolves to a recorded event, and a provider
// event supplies the conversation identity. Each refusal names its gate. Receipts are
// validated before the identity is read, so a corrupt newest event refuses under
// `request-unresolved` whether or not a receipt points at it.
fn inspect_reopen_source(directory: &Path, id: &str) -> Result<ReopenSource> {
    let manifest = Reader::open_unchecked(directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    verify_reopen_source_is_closed(directory, id)?;
    // The lifecycle readers' contract for `events`: it is a real directory inside the
    // session, or absent, before anything under it is opened. A link or a non-directory
    // planted there would carry the reads below outside the session, and the shared
    // listing would present it as "no events", which the identity gate would then report
    // as a source without an identity. Neither can prove what the source delivered. An
    // absent directory is not rejected here: it holds no event, so the receipt and identity
    // gates below refuse for what is actually missing.
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
            .record(CoreRecord::Events)
            .path()
            .to_owned()
            .join(&receipt.event_file);
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

// A reopen marker that names a session whose launch was refused and whose cleanup is now
// verified. The marker no longer excludes anything: the reopen it recorded delivered no
// prompt and its process is provably gone, so the next claim under the source lock removes
// it before writing its own.
#[derive(Debug)]
struct StaleReopenMarker {
    refused_session: String,
    gate: String,
    cleanup: RefusedLaunchCleanup,
}

// Read-only. The source is closed with its tombstone, nothing of its lifecycle is left
// open, and any reopen marker it carries is either stale (returned, so the claim can
// release it) or refuses under `already-reopened` naming the blocking condition. A marker
// is stale only when the session it names carries a durable launch-phase refusal and that
// refused launch is verified unable to hold the conversation (`refused_launch_cleanup`).
// This is how a marker whose parent reopen crashed before settlement, or whose launch
// wrapper recorded its refusal only after the parent timed out, is reconciled: nothing is
// inferred from the absence of records, and a marker that names a session without a launch
// refusal, or with a launch refusal whose process may survive, stays consumed.
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

// A recorded event that cannot be read is a turn whose outcome cannot be verified, so it
// refuses under `request-unresolved` even when no receipt points at it (legacy sessions).
// Every recorded event is read, not only those newer than the identity that is returned: an
// older unreadable event is as unverifiable as a newer one.
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

// A resumed session's conversation is re-checked immediately before every follow-up
// delivery. A refusal sends nothing: the claim is released and the reason is recorded in
// one status write, so the session stays ready with the refusal in its status and its
// receipt stays unresolved. This is the same best-effort detection the launch ran; a
// foreign resume that registers after this point is caught only by the next delivery.
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

// The claim is released and the refusal reason is published in one write under the
// lifecycle lock, and only while the claim is still this request's. Once another `tell`
// owns the turn, nothing is written: that turn keeps its claim, receipt, and status, and
// the refused request's receipt stays unresolved.
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

// The reopen marker of a closed source session: which reopen consumed it and whether the
// next reopen can proceed. Read-only, and the same judgment the reopen gate makes under the
// source lock (`refused_launch_cleanup`, which verifies the recorded provider process, not
// the launch wrapper or the closed surface); doctor neither releases nor annotates the
// marker.
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
