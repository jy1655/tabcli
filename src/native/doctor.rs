//! Observations and advice only. No recovery, delivery, terminal control, or settings writes.
use super::args::{option_value, set_flag_once, set_once};
use super::diagnostics::{Availability, Check, Context};
use super::query::observation::judgment::{
    self, ActiveHandle, BoundOwner, Cancel, Hold, Lifecycle, OwnerJudgment, ResultCandidate,
    Surface, SurfacePresence, SurfaceSource, Turn,
};
use super::query::observation::{Observation, PartialObservation, SessionEvidence};
use super::{NativeCommand, consent, provider, query, reopen, terminal};
use crate::native::clock::SystemClock;
use crate::native::diagnostics::vocabulary::{CheckId, Reason};
use crate::native::process_env::{PROBE_TIMEOUT, probe};
use crate::native::query::request::RequestState;
use crate::native::session::Reader;
use crate::native::session::launch::wrapper::{
    is_executable, provider_version_text, resolve_provider,
};
use crate::native::session::origin::{OriginEnvironment, OriginObservation, SourceKind};
use crate::native::session::primitives::{require_valid_session_id, unix_ms};
use crate::native::session::schema::SessionManifest;
use agent_bridge::PUBLIC_COMMAND;
use agent_bridge::{FirstPartyCli, cli_version_is_supported, terminal_safe_text};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::str::FromStr;
use std::time::Instant;

#[derive(Debug)]
pub(crate) struct DoctorRequest {
    session: Option<String>,
    provider: Option<FirstPartyCli>,
    probe: bool,
    json: bool,
}

pub(super) fn parse_args(args: &[String]) -> Result<NativeCommand> {
    let mut request = DoctorRequest {
        session: None,
        provider: None,
        probe: false,
        json: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                let value = option_value(args, &mut index, "--provider")?;
                set_once(
                    &mut request.provider,
                    FirstPartyCli::from_str(value).map_err(anyhow::Error::msg)?,
                    "--provider",
                )?;
            }
            "--probe" => set_flag_once(&mut request.probe, "--probe")?,
            "--json" => set_flag_once(&mut request.json, "--json")?,
            value if !value.starts_with('-') => {
                require_valid_session_id(value)?;
                set_once(&mut request.session, value.to_owned(), "session")?;
            }
            value => bail!("unknown doctor option: {value}"),
        }
        index += 1;
    }
    if request.session.is_some() == request.provider.is_some() {
        bail!("doctor requires either one session id or --provider <codex|claude|agy|pi>");
    }
    Ok(NativeCommand::Doctor(request))
}

pub(super) fn run(request: DoctorRequest) -> Result<()> {
    let started = unix_ms();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let diagnosis = assemble_checks(&request, deadline)?;
    render_report(&request, started, diagnosis)
}

struct Diagnosis {
    provider: Option<FirstPartyCli>,
    manifest: Option<SessionManifest>,
    scope: Value,
    observations: Value,
    checks: Vec<Check>,
}

// Keep record reads and probes interleaved in their original order. record_checks
// releases its snapshot before any surface or provider probe uses the shared deadline.
fn assemble_checks(request: &DoctorRequest, deadline: Instant) -> Result<Diagnosis> {
    use Availability::*;
    let mut checks = vec![Check::new(
        CheckId::PLATFORM,
        if cfg!(any(target_os = "macos", windows)) {
            Available
        } else {
            Unavailable
        },
        if cfg!(any(target_os = "macos", windows)) {
            Reason::PLATFORM_SUPPORTED
        } else {
            Reason::PLATFORM_UNSUPPORTED
        },
        format!("Managed terminals on {}", std::env::consts::OS),
        "Managed launch and delivery require a supported platform; stored records can still be inspected.",
    )];
    let directory = request
        .session
        .as_ref()
        .and_then(|id| match Reader::session_directory(id) {
            Ok(directory) => Some(directory),
            Err(error) => {
                checks.push(Check::new(
                    CheckId::SESSION_RECORDS,
                    Unavailable,
                    Reason::SESSION_UNAVAILABLE,
                    format!("{error:#}"),
                    "Check the explicit session id and state directory.",
                ));
                unknown_session_checks(id, Reason::SESSION_UNAVAILABLE, &mut checks);
                cancel_check(
                    &Cancel::Unreadable("session unavailable".to_owned()),
                    id,
                    &mut checks,
                );
                unobserved_origin_check(id, &mut checks);
                hold_check(
                    &Hold::Unreadable("session unavailable".to_owned()),
                    id,
                    &mut checks,
                );
                None
            }
        });
    let mut observations = json!({});
    if request.provider.is_some() {
        let selected = terminal::select(None);
        let (availability, reason, evidence) = match selected {
            Ok(kind) if kind.supported_on_this_platform() => (
                Available,
                Reason::TERMINAL_SELECTED,
                json!({"terminal": kind}),
            ),
            Ok(kind) => (
                Unavailable,
                Reason::TERMINAL_UNSUPPORTED,
                json!({"terminal": kind}),
            ),
            Err(error) => (
                Unavailable,
                Reason::TERMINAL_SELECTION_FAILED,
                json!({"error": format!("{error:#}")}),
            ),
        };
        checks.push(Check::new(CheckId::TERMINAL_SELECTION, availability, reason,
            "Default terminal selection from the launch adapter; no app was opened or controlled.",
            "An explicit ask --terminal can select another supported host; live surface availability has not been tested.").evidence(evidence));
        #[cfg(windows)]
        checks.push(match terminal::windows_powershell_executable() {
            Ok(path) => Check::new(
                CheckId::POWERSHELL,
                Available,
                Reason::POWERSHELL_FOUND,
                "PowerShell 7 executable was resolved.",
                "The managed console is created only by ask.",
            )
            .evidence(json!({"path": path})),
            Err(error) => Check::new(
                CheckId::POWERSHELL,
                Unavailable,
                Reason::POWERSHELL_UNAVAILABLE,
                format!("{error:#}"),
                "Install PowerShell 7 and check absolute PATH entries.",
            ),
        });
    }
    let manifest = directory.as_ref().and_then(|directory| {
        let mut clock = SystemClock;
        let (manifest, evidence) = record_checks(
            &Reader::open_unchecked(directory),
            request.session.as_deref().unwrap(),
            deadline,
            &mut checks,
            &mut observations,
            &mut OriginEnvironment::production(&mut clock),
        );
        if request.probe {
            surface_check(
                &evidence,
                &Reader::open_unchecked(directory),
                deadline,
                &mut checks,
            );
        }
        reopen::marker_check(
            &Reader::open_unchecked(directory),
            request.session.as_deref().unwrap(),
            &mut checks,
        );
        manifest
    });
    let provider = match manifest
        .as_ref()
        .map(|m| FirstPartyCli::from_str(&m.provider))
    {
        Some(Ok(provider)) => Some(provider),
        Some(Err(error)) => {
            checks.push(Check::new(
                CheckId::PROVIDER,
                Unknown,
                Reason::PROVIDER_UNRECOGNIZED,
                error,
                "Inspect the recorded provider; do not guess a replacement.",
            ));
            None
        }
        None => request.provider,
    };
    let mut executable = None;
    let mut current_version = None;
    let mut scope_workspace = None;
    let mut consent_state = Value::Null;
    if let Some(provider) = provider {
        let resolved = match &manifest {
            Some(manifest) => Ok(manifest.provider_path.clone()),
            None => resolve_provider(provider),
        };
        match resolved {
            Ok(path) if path.is_absolute() && path.is_file() && is_executable(&path) => {
                checks.push(Check::new(CheckId::PROVIDER_EXECUTABLE, Available, Reason::EXECUTABLE_FOUND, "Provider executable found; presence alone does not establish runtime capability.", "Use --probe to read its current version.").evidence(json!({"path": path})));
                executable = Some(path);
            }
            Ok(path) if !path.is_absolute() => {
                checks.push(Check::new(CheckId::PROVIDER_EXECUTABLE, Unknown, Reason::EXECUTABLE_PATH_UNVERIFIED, "The recorded executable path is relative; doctor does not substitute a PATH lookup for it.", "Inspect the original launch configuration.").evidence(json!({"path": path})));
            }
            outcome => {
                let detail = match outcome {
                    Ok(path) => format!(
                        "Provider executable is missing, not executable, or not absolute: {}",
                        path.display()
                    ),
                    Err(error) => format!("{error:#}"),
                };
                checks.push(Check::new(CheckId::PROVIDER_EXECUTABLE, Unavailable, Reason::EXECUTABLE_UNAVAILABLE, detail, "Check the provider installation and PATH; existing sessions retain their recorded executable path."));
            }
        }
        if request.probe {
            if let Some(path) = &executable {
                match probe(
                    path,
                    &["--version"],
                    None,
                    provider::probe_environment_removals(provider),
                    deadline,
                ) {
                    Ok(output) if output.status.success() => {
                        let version = provider_version_text(&output.stdout, &output.stderr);
                        let (availability, reason) =
                            match cli_version_is_supported(provider, &version) {
                                Ok(true) => (Available, Reason::VERSION_SUPPORTED),
                                Ok(false) => (Unavailable, Reason::VERSION_UNSUPPORTED),
                                Err(_) => (Unknown, Reason::VERSION_UNRECOGNIZED),
                            };
                        checks.push(Check::new(CheckId::PROVIDER_VERSION, availability, reason, format!("Current CLI version; required minimum {}", provider.minimum_version()), "CLI version is only one gate; inspect the provider-specific checks.").evidence(json!({"current_version": version})));
                        current_version = Some(version);
                    }
                    outcome => {
                        let detail = match outcome {
                            Ok(output) => format!("Version probe exited with {}", output.status),
                            Err(error) => format!("{error:#}"),
                        };
                        checks.push(Check::new(CheckId::PROVIDER_VERSION, Unknown, Reason::VERSION_PROBE_FAILED, detail, "Inspect the provider installation; a failed probe does not prove availability."));
                    }
                }
            } else {
                checks.push(Check::new(
                    CheckId::PROVIDER_VERSION,
                    Unknown,
                    Reason::EXECUTABLE_UNAVAILABLE,
                    "No unambiguous executable is available for a version probe.",
                    "Inspect the provider_executable check.",
                ));
            }
        } else {
            checks.push(Check::new(CheckId::PROVIDER_VERSION, Unknown, Reason::PROBE_NOT_REQUESTED, "Current CLI version was not queried. A launch-recorded version is historical evidence only.", "Add --probe for bounded local version checks; no model or message is sent.").evidence(json!({"minimum_version": provider.minimum_version().to_string()})));
        }
        let workspace = manifest
            .as_ref()
            .map(|m| m.workspace.clone())
            .map_or_else(std::env::current_dir, Ok)?;
        let (availability, reason) = match fs::metadata(&workspace) {
            Ok(metadata) if metadata.is_dir() => (Available, Reason::WORKSPACE_PRESENT),
            Ok(_) => (Unavailable, Reason::WORKSPACE_NOT_DIRECTORY),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Unavailable, Reason::WORKSPACE_MISSING)
            }
            Err(_) => (Unknown, Reason::WORKSPACE_UNREADABLE),
        };
        checks.push(Check::new(CheckId::WORKSPACE, availability, reason, "Working directory observation; CLI version checks do not depend on this directory.", "Restore or inspect the recorded directory before workspace-relative operations.").evidence(json!({"path": workspace})));
        if let Some(directory) = &directory {
            let consent = consent::observe(directory);
            consent_state = consent.value()["state"].clone();
            let availability = if consent.state() == Some(consent::ConsentState::Verified) {
                Available
            } else {
                Unknown
            };
            checks.push(Check::new(CheckId::WORKSPACE_CONSENT, availability, Reason::RECORDED_WORKSPACE_CONSENT,
                "Launch-time workspace consent and its provider-owned source. This is historical evidence, not a new trust grant.",
                "Inspect workspace_consent; consent revoke disables sharing, and consent reset permits fresh provider evidence on a later ask.").evidence(consent.into_value()));
        }
        let environment = consent::Environment::current();
        checks.extend(provider::diagnose(
            provider,
            Context {
                directory: directory.as_deref(),
                manifest: manifest.as_ref(),
                executable: executable.as_deref(),
                current_version: current_version.as_deref(),
                workspace: &workspace,
                probe: request.probe,
                deadline,
                environment: &environment,
            },
        ));
        scope_workspace = Some(workspace);
    }
    let workspace = scope_workspace
        .or_else(|| manifest.as_ref().map(|m| m.workspace.clone()))
        .or_else(|| std::env::current_dir().ok());
    let scope = json!({
        "session": request.session,
        "provider": provider.map(|p| p.as_str()),
        "executable": executable,
        "executable_source": executable.as_ref().map(|_| {
            if manifest.is_some() { "launch_record" } else { "resolved_path" }
        }),
        "current_version": current_version,
        "model": manifest.as_ref().and_then(|m| m.model.as_deref()),
        "effort": manifest.as_ref().and_then(|m| m.effort.as_deref()),
        "workspace": workspace,
        "probe": request.probe,
        "probe_budget_ms": PROBE_TIMEOUT.as_millis(),
        "consent_state": consent_state,
    });
    Ok(Diagnosis {
        provider,
        manifest,
        scope,
        observations,
        checks,
    })
}

fn render_report(request: &DoctorRequest, started: u128, diagnosis: Diagnosis) -> Result<()> {
    use Availability::*;
    let Diagnosis {
        provider,
        manifest,
        scope,
        observations,
        checks,
    } = diagnosis;
    let report = json!({
        "schema_version": 1, "ok": true, "session": request.session,
        "provider": provider.map(|p| p.as_str()), "probe": request.probe,
        "scope": scope,
        "started_unix_ms": started, "finished_unix_ms": unix_ms(),
        "configured": manifest.as_ref().map(|m| json!({
            "workspace": m.workspace, "provider_path": m.provider_path,
            "provider_version_at_launch": m.provider_version,
            "model": m.model, "effort": m.effort, "yolo": m.yolo,
        })),
        "observations": observations, "checks": checks,
    });
    if request.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "scope: {}",
        terminal_safe_text(&report["scope"].to_string(), false)
    );
    println!(
        "doctor: {}",
        request
            .session
            .as_deref()
            .unwrap_or_else(|| provider.map_or("unknown", |p| p.as_str()))
    );
    for key in ["configured", "observations"] {
        if !report[key].is_null() {
            println!(
                "{key}: {}",
                terminal_safe_text(&report[key].to_string(), false)
            );
        }
    }
    for check in &checks {
        println!(
            "[{}] {} ({})\n  {}\n  next: {}",
            match check.availability {
                Available => "available",
                Unavailable => "unavailable",
                Unknown => "unknown",
            },
            check.id,
            check.reason_code,
            terminal_safe_text(&check.detail, false),
            terminal_safe_text(&check.next_action, false)
        );
        if !check.evidence.is_null() {
            println!(
                "  evidence: {}",
                terminal_safe_text(&check.evidence.to_string(), false)
            );
        }
        if let Some(command) = &check.next_command {
            println!(
                "  command argv: {}",
                terminal_safe_text(&serde_json::to_string(command)?, false)
            );
        }
    }
    Ok(())
}

pub(super) fn record_checks(
    reader: &Reader,
    id: &str,
    deadline: Instant,
    checks: &mut Vec<Check>,
    observations: &mut Value,
    environment: &mut OriginEnvironment<'_>,
) -> (Option<SessionManifest>, SessionEvidence) {
    let (manifest, evidence, surface, hold) = match Observation::read(reader) {
        Ok(observation) => {
            session_checks(&observation, checks, observations);
            cancel_check(&observation.judgments.cancel, id, checks);
            let origin = observation.observe_origin(reader, Some(deadline), environment);
            origin.add_fields(observations);
            origin_check(
                &origin,
                observation.judgments.lifecycle == Lifecycle::Closed,
                id,
                checks,
            );
            let Observation {
                records,
                evidence,
                judgments,
                ..
            } = observation;
            (
                Some(records.manifest),
                evidence,
                judgments.surface,
                judgments.hold,
            )
        }
        Err(error) => {
            let reason = if error.is::<query::SnapshotBusy>() {
                Reason::RECORDS_BUSY
            } else {
                Reason::RECORDS_UNREADABLE
            };
            checks.push(Check::new(CheckId::SESSION_RECORDS, Availability::Unknown, reason, format!("{error:#}"), "Inspect the records without deleting locks or resending a request; retry if a writer is active."));
            unknown_session_checks(id, reason, checks);
            let PartialObservation {
                manifest,
                evidence,
                cancel,
                surface,
                hold,
            } = PartialObservation::read(reader, id);
            cancel_check(&cancel, id, checks);
            unobserved_origin_check(id, checks);
            (manifest, evidence, surface, hold)
        }
    };
    owner_check(evidence.judge_owner(id), checks);
    terminal_check(&surface, checks);
    hold_check(&hold, id, checks);
    (manifest, evidence)
}

fn cancel_check(cancel: &Cancel, id: &str, checks: &mut Vec<Check>) {
    let (availability, reason, detail) = match cancel {
        Cancel::Absent => (
            Availability::Available,
            Reason::CANCEL_ABSENT,
            "No cancel request is recorded.",
        ),
        Cancel::Unreadable(_) => (
            Availability::Unknown,
            Reason::CANCEL_UNREADABLE,
            "The cancel request cannot be read reliably; published results remain independent.",
        ),
        Cancel::Recorded(_) => (
            Availability::Available,
            Reason::CANCEL_RECORDED,
            "A cancel request is recorded; only a correlated provider result confirms interruption.",
        ),
    };
    checks.push(Check::new(
        CheckId::CANCEL,
        availability,
        reason,
        detail,
        format!("{PUBLIC_COMMAND} inspect {id} --json"),
    ));
}

/// The session's Origin as its records show it. Never a close authority: an ended Origin
/// only suggests an explicit close of this session when the user no longer needs it.
fn origin_check(
    origin: &OriginObservation,
    session_closed: bool,
    id: &str,
    checks: &mut Vec<Check>,
) {
    let inspect = format!("{PUBLIC_COMMAND} inspect {id} --json");
    let check = match (origin.reference.kind, origin.ended) {
        (SourceKind::External, _) => Check::new(CheckId::ORIGIN,
            Availability::Available,
            Reason::ORIGIN_EXTERNAL,
            "The initial request came from outside Agent Bridge; there is no managed origin to observe.",
            "No action is needed.",
        ),
        (_, Some(false)) => Check::new(CheckId::ORIGIN,
            Availability::Available,
            Reason::ORIGIN_LIVE,
            "The origin session's records and its attested owner show it has not ended.",
            "No action is needed.",
        ),
        (_, Some(true)) if session_closed => Check::new(CheckId::ORIGIN,
            Availability::Available,
            Reason::ORIGIN_ENDED_SESSION_CLOSED,
            "The origin session has ended, and this session is already closed.",
            "No action is needed.",
        ),
        (_, Some(true)) => Check::new(CheckId::ORIGIN,
            Availability::Unavailable,
            Reason::ORIGIN_ENDED,
            "The origin session has ended in its records; this session is not closed. This is not evidence of an open surface.",
            "If this session is no longer needed, close it explicitly; the origin observation grants no close authority.",
        )
        .command(vec![
            PUBLIC_COMMAND.to_owned(),
            "close-session".to_owned(),
            id.to_owned(),
            "--explicit".to_owned(),
        ]),
        (_, None) => Check::new(CheckId::ORIGIN,
            Availability::Unknown,
            Reason::ORIGIN_UNVERIFIED,
            format!(
                "Whether the origin has ended could not be verified ({}).",
                origin.reason
            ),
            inspect,
        ),
    };
    let mut evidence = json!({});
    origin.add_fields(&mut evidence);
    checks.push(check.evidence(evidence));
}

fn hold_check(hold: &Hold, id: &str, checks: &mut Vec<Check>) {
    let check = match hold {
        Hold::NotHeld => Check::new(
            CheckId::HOLD,
            Availability::Available,
            Reason::NOT_HELD,
            "No follow-up hold is recorded.",
            "No hold release is needed.",
        ),
        Hold::Held { release_allowed } => Check::new(
            CheckId::HOLD,
            Availability::Unavailable,
            Reason::SESSION_HELD,
            "The user's hold refuses follow-ups whose delivery has not begun.",
            if *release_allowed {
                format!("{PUBLIC_COMMAND} hold {id} --release")
            } else {
                "Preserve the hold record; inspect the session's recorded lifecycle before taking action.".to_owned()
            },
        ),
        Hold::Unreadable(reason) => Check::new(
            CheckId::HOLD,
            Availability::Unknown,
            Reason::HOLD_UNREADABLE,
            reason.clone(),
            format!("{PUBLIC_COMMAND} inspect {id} --json"),
        ),
    };
    checks.push(check);
}

/// The origin check when no consistent snapshot of this session could be read.
fn unobserved_origin_check(id: &str, checks: &mut Vec<Check>) {
    checks.push(Check::new(
        CheckId::ORIGIN,
        Availability::Unknown,
        Reason::ORIGIN_UNVERIFIED,
        "No consistent session snapshot was available, so the origin was not observed.",
        format!("{PUBLIC_COMMAND} inspect {id} --json"),
    ));
}

fn unknown_session_checks(id: &str, reason: Reason, checks: &mut Vec<Check>) {
    for check_id in [CheckId::SESSION_STATE, CheckId::TURN, CheckId::COMPLETION] {
        checks.push(Check::new(check_id, Availability::Unknown, reason,
            "No consistent session snapshot was available; missing evidence is not a ready or completed state.",
            format!("{PUBLIC_COMMAND} result {id} --list --json")));
    }
}

fn session_checks(observation: &Observation, checks: &mut Vec<Check>, observations: &mut Value) {
    use Availability::*;
    let snapshot = &observation.records;
    let judgments = &observation.judgments;
    let active = observation.active_request();
    // Doctor decodes no latest event: every candidate but the active Request is the list.
    let result_action = match judgment::result_candidate(snapshot, None) {
        ResultCandidate::ActiveRequest(request) => format!(
            "{PUBLIC_COMMAND} result {} --request {request} --json",
            snapshot.manifest.id
        ),
        ResultCandidate::ClaimOnly | ResultCandidate::LatestDecoded(_) | ResultCandidate::None => {
            format!(
                "{PUBLIC_COMMAND} result {} --list --json",
                snapshot.manifest.id
            )
        }
    };
    let request_state = judgments.active_state.as_ref().map(|state| {
        state
            .as_ref()
            .map(|state| state.name())
            .unwrap_or(RequestState::Unknown.name())
    });
    *observations = json!({"stored_state": snapshot.status.state, "generation": snapshot.status.generation,
        "updated_unix_ms": snapshot.status.updated_unix_ms, "session_error": snapshot.status.error,
        "active_request_id": active.map(|r| &r.request_id), "active_request_state": request_state,
        "recovery_required": judgments.recovery_required});
    if judgments.claim_without_receipt {
        checks.push(Check::new(CheckId::ACTIVE_REQUEST, Unknown, Reason::ACTIVE_REQUEST_UNKNOWN, "The active claim has no readable request receipt; its public request identity is unknown.", &result_action));
    }
    checks.push(Check::new(
        CheckId::SESSION_RECORDS,
        Available,
        Reason::RECORDS_READ,
        "Observed stored state without recovery; this is not a live delivery check.",
        "Use inspect for recorded launch and result details.",
    ));
    checks.push(Check::new(
        CheckId::SESSION_STATE,
        if judgments.lifecycle == Lifecycle::Ready {
            Available
        } else {
            Unavailable
        },
        Reason::STORED_STATE,
        format!("Stored state: {}", snapshot.status.state),
        &result_action,
    ));
    let (availability, reason, detail) = match &judgments.turn {
        Turn::LaunchFailed { reason, detail } => (
            Unavailable,
            Reason::ALL
                .iter()
                .copied()
                .find(|word| word.name() == *reason)
                .expect("known launch reason"),
            detail.as_str(),
        ),
        Turn::DeliveryUnconfirmed => (
            Unknown,
            Reason::DELIVERY_UNCONFIRMED,
            "A turn remains claimed with an error. Delivery may have occurred; do not resend.",
        ),
        Turn::TurnInProgress => (
            Unavailable,
            Reason::TURN_IN_PROGRESS,
            "A turn is claimed; wait for the same request result instead of sending another turn.",
        ),
        Turn::NoActiveTurn => (
            Available,
            Reason::NO_ACTIVE_TURN,
            "No active turn claim was observed.",
        ),
    };
    checks.push(Check::new(
        CheckId::TURN,
        availability,
        reason,
        detail,
        &result_action,
    ));
    let mut completion = Check::new(
        CheckId::COMPLETION,
        if judgments.recovery_required {
            Unavailable
        } else {
            Available
        },
        if judgments.recovery_required {
            Reason::RECOVERY_REQUIRED
        } else {
            Reason::NO_PENDING_COMPLETION
        },
        "Completion journal observation; doctor never publishes or repairs a result.",
        if judgments.recovery_required {
            "The suggested sessions command changes state: it recovers completion and can close dead-owner sessions in that workspace. Run it explicitly, then query the same request."
        } else {
            "Retrieve recorded results with result."
        },
    );
    if judgments.recovery_required {
        completion = completion.command(vec![
            PUBLIC_COMMAND.to_owned(),
            "sessions".to_owned(),
            "--workspace".to_owned(),
            snapshot.manifest.workspace.to_string_lossy().into_owned(),
            "--json".to_owned(),
        ]);
    }
    checks.push(completion);
    if let Some(index) = &judgments.request_index {
        checks.push(Check::new(CheckId::REQUEST_INDEX, Unknown, Reason::REQUEST_INDEX_INCOMPLETE, "Some request references could not be read; absence is not proof that no request exists.", "Use an exact event id when known; preserve damaged request records.")
            .evidence(json!({"unreadable_requests": index.unreadable, "error": index.error})));
    }
}

fn surface_check(
    evidence: &SessionEvidence,
    reader: &Reader,
    deadline: Instant,
    checks: &mut Vec<Check>,
) {
    checks.push(surface_presence_check(
        evidence.judge_surface_presence(reader, deadline),
    ));
}

/// The doctor mapping of the on-request surface presence judgment.
pub(in crate::native) fn surface_presence_check(presence: &SurfacePresence) -> Check {
    use Availability::*;
    let (availability, reason, detail) = match presence {
        SurfacePresence::Present => (Available, Reason::TERMINAL_SURFACE_PRESENT, "Recorded terminal surface is present; this does not prove provider readiness or delivery.".to_owned()),
        SurfacePresence::Missing => (Unavailable, Reason::TERMINAL_SURFACE_MISSING, "Recorded terminal surface is absent; a provider process may still survive it.".to_owned()),
        SurfacePresence::Unverified(reason) => (Unknown, Reason::TERMINAL_SURFACE_UNVERIFIED, format!("Surface presence could not be verified: {reason}")),
    };
    Check::new(
        CheckId::TERMINAL_SURFACE,
        availability,
        reason,
        detail,
        "Inspect the launch log and exact request; this read-only probe does not release a claim or resend input.",
    )
}

fn owner_check(owner: &OwnerJudgment, checks: &mut Vec<Check>) {
    use Availability::*;
    let (availability, reason, detail, evidence) = match owner {
        OwnerJudgment::Bound(attestation) => {
            // An Unknown attestation is unverified whatever its PID hint (B3).
            let (availability, reason) = match BoundOwner::of(attestation) {
                BoundOwner::Exited => (Unavailable, Reason::OWNER_EXITED),
                BoundOwner::Reused | BoundOwner::ControlChanged => {
                    (Unavailable, Reason::OWNER_IDENTITY_MISMATCH)
                }
                BoundOwner::Verified => (Available, Reason::OWNER_IDENTITY_OBSERVED),
                BoundOwner::Uncompared | BoundOwner::Unknown => (Unknown, Reason::OWNER_UNVERIFIED),
            };
            (
                availability,
                reason,
                "Owner observation is separate from terminal surface or provider readiness."
                    .to_owned(),
                json!(owner.observation()),
            )
        }
        OwnerJudgment::Foreign => (
            Unavailable,
            Reason::OWNER_SESSION_MISMATCH,
            "Owner record belongs to another managed session.".to_owned(),
            Value::Null,
        ),
        OwnerJudgment::UnboundLegacy { .. } => (Unknown, Reason::OWNER_UNBOUND_LEGACY, "A legacy owner record has no managed-session binding; process observation is not proof of control authority.".to_owned(), json!(owner.observation())),
        OwnerJudgment::Absent => (
            Unknown,
            Reason::OWNER_UNVERIFIED,
            "No owner record bound to this managed session was observed.".to_owned(),
            Value::Null,
        ),
        OwnerJudgment::Unreadable(error) => (
            Unknown,
            Reason::OWNER_UNREADABLE,
            error.clone(),
            Value::Null,
        ),
    };
    checks.push(Check::new(CheckId::OWNER, availability, reason, detail, "Inspect the exact request result; do not infer delivery or resend from owner liveness.").evidence(evidence));
}

fn terminal_check(surface: &Surface, checks: &mut Vec<Check>) {
    use Availability::*;
    const ACTIVE_NEXT: &str = "Only tell/close revalidate their target surface; this observation grants no control authority.";
    let check = match surface {
        Surface::Closed { consumed } => Check::new(
            CheckId::TERMINAL_RECORD,
            if *consumed { Unavailable } else { Unknown },
            if *consumed {
                Reason::TERMINAL_CONSUMED
            } else {
                Reason::TERMINAL_TOMBSTONE_UNREADABLE
            },
            "A terminal tombstone is recorded; a consumed handle cannot be reused.",
            "Inspect retained results; doctor does not reopen or delete terminal records.",
        ),
        Surface::Unreadable {
            source: SurfaceSource::ClosedTombstone,
            reason,
        } => Check::new(
            CheckId::TERMINAL_RECORD,
            Unknown,
            Reason::TERMINAL_TOMBSTONE_UNREADABLE,
            reason.clone(),
            "Preserve the terminal records and inspect the close attempt.",
        ),
        Surface::Closing { bound } => Check::new(
            CheckId::TERMINAL_RECORD,
            if *bound { Unavailable } else { Unknown },
            if *bound {
                Reason::TERMINAL_CLOSE_IN_PROGRESS
            } else {
                Reason::TERMINAL_BINDING_UNVERIFIED
            },
            "A claimed close handle remains; the close may still be running or require explicit recovery.",
            "Inspect the prior close attempt. Finishing it requires an explicit close request and ownership verification.",
        ),
        Surface::Unreadable {
            source: SurfaceSource::ClosingHandle,
            reason,
        } => Check::new(
            CheckId::TERMINAL_RECORD,
            Unknown,
            Reason::TERMINAL_RECORD_UNREADABLE,
            reason.clone(),
            "Preserve and inspect the close handle.",
        ),
        Surface::Active {
            kind,
            supported,
            bound,
            empty_id,
        } => {
            let (availability, reason) = match ActiveHandle::of(*supported, *bound, *empty_id) {
                ActiveHandle::Unsupported => (Unavailable, Reason::TERMINAL_UNSUPPORTED),
                ActiveHandle::BindingUnverified => (Unknown, Reason::TERMINAL_BINDING_UNVERIFIED),
                ActiveHandle::Bound => (Available, Reason::TERMINAL_RECORD_FOUND),
            };
            Check::new(
                CheckId::TERMINAL_RECORD,
                availability,
                reason,
                "Stored terminal metadata only; no live surface control was attempted.",
                ACTIVE_NEXT,
            )
            .evidence(json!({"terminal": kind}))
        }
        Surface::Missing => Check::new(
            CheckId::TERMINAL_RECORD,
            Unavailable,
            Reason::TERMINAL_RECORD_MISSING,
            "No active terminal handle is recorded.",
            ACTIVE_NEXT,
        ),
        Surface::Unreadable {
            source: SurfaceSource::ActiveHandle,
            reason,
        } => Check::new(
            CheckId::TERMINAL_RECORD,
            Unknown,
            Reason::TERMINAL_RECORD_UNREADABLE,
            reason.clone(),
            ACTIVE_NEXT,
        ),
    };
    checks.push(check);
}

#[cfg(test)]
mod attestation_tests {
    use super::*;
    use crate::native::process_identity::{Attested, Gone, ProcessObservation};
    use crate::native::query::observation::judgment::{self, tests::attestation};
    use crate::native::session::owner::ControlAttestation;
    use crate::native::session::owner::tests::reused_attestation;

    /// The owner check as [availability, reason, evidence facts], the facts as an array.
    fn owner_row(owner: &OwnerJudgment) -> Value {
        let mut checks = vec![];
        owner_check(owner, &mut checks);
        assert_eq!(checks.len(), 1);
        let check = serde_json::to_value(&checks[0]).unwrap();
        let evidence = &check["evidence"];
        let facts = if evidence.is_null() {
            Value::Null
        } else {
            json!([
                evidence["process_alive"],
                evidence["identity_matches"],
                evidence["error"]
            ])
        };
        json!([check["availability"], check["reason_code"], facts])
    }

    fn bound_evidence(store: &crate::native::session::Store) -> SessionEvidence {
        store
            .write_owner(&crate::native::terminal::ownership::NativeSessionOwner {
                pid: 4242,
                managed_session_id: Some("session-test".into()),
                terminal_tty_device: Some(9),
                process_start_seconds: Some(10),
                process_start_microseconds: Some(20),
                ..Default::default()
            })
            .unwrap();
        SessionEvidence::read(store, Ok(false))
    }

    fn observation(records: crate::native::query::Snapshot) -> Observation {
        use crate::native::query::observation::judgment::tests::{SESSION, evidence};
        let evidence = evidence();
        let judgments =
            judgment::Judgments::of(&records, &evidence, None, &Ok(None), None, None, SESSION);
        Observation {
            origin: crate::native::session::origin::reference(&records),
            cancel: None,
            records,
            evidence,
            resumed_from: Ok(None),
            workspace_consent: consent::SessionConsentObservation::default(),
            judgments,
        }
    }

    fn find<'a>(checks: &'a [Check], id: &str) -> Option<&'a Check> {
        checks.iter().find(|check| check.id == id)
    }

    /// A check as [availability, reason].
    fn row(check: &Check) -> Value {
        json!([
            serde_json::to_value(check.availability).unwrap(),
            check.reason_code
        ])
    }

    #[test]
    fn doctor_maps_each_judgment_to_its_documented_check() {
        use crate::native::query::observation::judgment::tests::{
            CLAIM, SESSION, receipt, snapshot,
        };
        use crate::native::query::observation::judgment::{
            CancelState, IndexIncomplete, SurfacePresence,
        };
        use crate::native::query::request::RequestState;
        use crate::native::terminal::TerminalKind;
        use ControlAttestation::*;

        // session_checks: the turn, the completion, the index, the claim and the actions.
        let mut records = snapshot(crate::native::session::SessionState::Working);
        records.claim = Some(CLAIM.to_owned());
        records.receipts.push(receipt(CLAIM));
        let mut observation = observation(records);
        let session = |observation: &Observation| {
            let mut checks = Vec::new();
            let mut observations = json!({});
            session_checks(observation, &mut checks, &mut observations);
            (checks, observations)
        };
        for (turn, expected, detail) in [
            (
                Turn::LaunchFailed {
                    reason: "launch_uncertain",
                    detail: "spawn interrupted".into(),
                },
                json!(["unavailable", "launch_uncertain"]),
                "spawn interrupted",
            ),
            (
                Turn::DeliveryUnconfirmed,
                json!(["unknown", "delivery_unconfirmed"]),
                "A turn remains claimed with an error. Delivery may have occurred; do not resend.",
            ),
            (
                Turn::TurnInProgress,
                json!(["unavailable", "turn_in_progress"]),
                "A turn is claimed; wait for the same request result instead of sending another turn.",
            ),
            (
                Turn::NoActiveTurn,
                json!(["available", "no_active_turn"]),
                "No active turn claim was observed.",
            ),
        ] {
            observation.judgments.turn = turn.clone();
            let (checks, _) = session(&observation);
            let turn_check = find(&checks, "turn").unwrap();
            assert_eq!(row(turn_check), expected, "{turn:?}");
            assert_eq!(turn_check.detail, detail, "{turn:?}");
            assert!(
                turn_check
                    .next_action
                    .ends_with(&format!("result {SESSION} --request request-test --json"))
            );
        }
        for (recovery, expected, command) in [
            (true, json!(["unavailable", "recovery_required"]), true),
            (false, json!(["available", "no_pending_completion"]), false),
        ] {
            observation.judgments.recovery_required = recovery;
            let (checks, observations) = session(&observation);
            let completion = find(&checks, "completion").unwrap();
            assert_eq!(row(completion), expected, "{recovery}");
            assert_eq!(completion.next_command.is_some(), command);
            assert_eq!(observations["recovery_required"], recovery);
        }
        observation.judgments.request_index = Some(IndexIncomplete {
            unreadable: 2,
            error: Some("requests: not a directory".into()),
        });
        let (checks, _) = session(&observation);
        let index = find(&checks, "request_index").unwrap();
        assert_eq!(row(index), json!(["unknown", "request_index_incomplete"]));
        assert_eq!(
            index.evidence,
            json!({"unreadable_requests": 2, "error": "requests: not a directory"})
        );
        observation.judgments.request_index = None;
        assert!(find(&session(&observation).0, "request_index").is_none());
        for (state, expected) in [
            (None, Value::Null),
            (Some(Ok(RequestState::Completed)), json!("completed")),
            (
                Some(Err(anyhow::anyhow!("event: invalid"))),
                json!("unknown"),
            ),
        ] {
            observation.judgments.active_state = state;
            let (_, observations) = session(&observation);
            assert_eq!(observations["active_request_state"], expected);
            assert_eq!(observations["active_request_id"], "request-test");
        }
        assert!(find(&session(&observation).0, "active_request").is_none());
        observation.judgments.claim_without_receipt = true;
        observation.records.receipts.clear();
        let (checks, observations) = session(&observation);
        let active = find(&checks, "active_request").unwrap();
        assert_eq!(row(active), json!(["unknown", "active_request_unknown"]));
        assert!(
            active
                .next_action
                .ends_with(&format!("result {SESSION} --list --json"))
        );
        assert_eq!(observations["active_request_id"], Value::Null);
        assert_eq!(
            row(find(&checks, "session_state").unwrap()),
            json!(["unavailable", "stored_state"])
        );
        for (lifecycle, availability) in [
            (Lifecycle::Closed, "unavailable"),
            (Lifecycle::Failed, "unavailable"),
            (Lifecycle::Exited, "unavailable"),
            (Lifecycle::Other, "unavailable"),
            (Lifecycle::Ready, "available"),
        ] {
            observation.judgments.lifecycle = lifecycle;
            let (checks, _) = session(&observation);
            let check = find(&checks, "session_state").unwrap();
            assert_eq!(row(check), json!([availability, "stored_state"]));
            assert_eq!(check.detail, "Stored state: working");
        }

        // cancel_check
        for (cancel, expected) in [
            (Cancel::Absent, json!(["available", "cancel_absent"])),
            (
                Cancel::Recorded(Some(CancelState::RequestedActive)),
                json!(["available", "cancel_recorded"]),
            ),
            (
                Cancel::Recorded(Some(CancelState::NotApplied)),
                json!(["available", "cancel_recorded"]),
            ),
            (
                Cancel::Recorded(None),
                json!(["available", "cancel_recorded"]),
            ),
            (
                Cancel::Unreadable("x".into()),
                json!(["unknown", "cancel_unreadable"]),
            ),
        ] {
            let mut checks = Vec::new();
            cancel_check(&cancel, SESSION, &mut checks);
            assert_eq!(row(&checks[0]), expected, "{cancel:?}");
            assert_eq!(
                checks[0].next_action,
                format!("{PUBLIC_COMMAND} inspect {SESSION} --json")
            );
        }

        // hold_check: the release command only when the judgment allows it.
        for (hold, expected, release) in [
            (Hold::NotHeld, json!(["available", "not_held"]), false),
            (
                Hold::Held {
                    release_allowed: true,
                },
                json!(["unavailable", "session_held"]),
                true,
            ),
            (
                Hold::Held {
                    release_allowed: false,
                },
                json!(["unavailable", "session_held"]),
                false,
            ),
            (
                Hold::Unreadable("hold.json: invalid".into()),
                json!(["unknown", "hold_unreadable"]),
                false,
            ),
        ] {
            let mut checks = Vec::new();
            hold_check(&hold, SESSION, &mut checks);
            assert_eq!(row(&checks[0]), expected, "{hold:?}");
            assert_eq!(
                checks[0].next_action == format!("{PUBLIC_COMMAND} hold {SESSION} --release"),
                release,
                "{hold:?}"
            );
            if let Hold::Unreadable(reason) = &hold {
                assert_eq!(&checks[0].detail, reason);
            }
        }

        // terminal_check: the eight reasons from the surface judgment alone.
        let kind = TerminalKind::Warp;
        let active = |supported, bound, empty_id| Surface::Active {
            kind,
            supported,
            bound,
            empty_id,
        };
        let unreadable = |source| Surface::Unreadable {
            source,
            reason: "record: invalid".into(),
        };
        for (surface, expected, evidence) in [
            (
                Surface::Closed { consumed: true },
                json!(["unavailable", "terminal_consumed"]),
                Value::Null,
            ),
            (
                Surface::Closed { consumed: false },
                json!(["unknown", "terminal_tombstone_unreadable"]),
                Value::Null,
            ),
            (
                unreadable(SurfaceSource::ClosedTombstone),
                json!(["unknown", "terminal_tombstone_unreadable"]),
                Value::Null,
            ),
            (
                Surface::Closing { bound: true },
                json!(["unavailable", "terminal_close_in_progress"]),
                Value::Null,
            ),
            (
                Surface::Closing { bound: false },
                json!(["unknown", "terminal_binding_unverified"]),
                Value::Null,
            ),
            (
                unreadable(SurfaceSource::ClosingHandle),
                json!(["unknown", "terminal_record_unreadable"]),
                Value::Null,
            ),
            (
                active(false, false, false),
                json!(["unavailable", "terminal_unsupported"]),
                json!({"terminal": "warp"}),
            ),
            (
                active(true, false, false),
                json!(["unknown", "terminal_binding_unverified"]),
                json!({"terminal": "warp"}),
            ),
            (
                active(true, true, true),
                json!(["unknown", "terminal_binding_unverified"]),
                json!({"terminal": "warp"}),
            ),
            (
                active(true, true, false),
                json!(["available", "terminal_record_found"]),
                json!({"terminal": "warp"}),
            ),
            (
                Surface::Missing,
                json!(["unavailable", "terminal_record_missing"]),
                Value::Null,
            ),
            (
                unreadable(SurfaceSource::ActiveHandle),
                json!(["unknown", "terminal_record_unreadable"]),
                Value::Null,
            ),
        ] {
            let mut checks = Vec::new();
            terminal_check(&surface, &mut checks);
            assert_eq!(checks[0].id, "terminal_record");
            assert_eq!(row(&checks[0]), expected, "{surface:?}");
            assert_eq!(checks[0].evidence, evidence, "{surface:?}");
            if let Surface::Unreadable { reason, .. } = &surface {
                assert_eq!(&checks[0].detail, reason);
            }
        }

        // surface_presence_check
        for (presence, expected) in [
            (
                SurfacePresence::Present,
                json!(["available", "terminal_surface_present"]),
            ),
            (
                SurfacePresence::Missing,
                json!(["unavailable", "terminal_surface_missing"]),
            ),
            (
                SurfacePresence::Unverified("probe deadline exhausted".into()),
                json!(["unknown", "terminal_surface_unverified"]),
            ),
        ] {
            let check = surface_presence_check(&presence);
            assert_eq!(check.id, "terminal_surface");
            assert_eq!(row(&check), expected, "{presence:?}");
            if let SurfacePresence::Unverified(reason) = &presence {
                assert!(check.detail.ends_with(reason));
            }
        }

        // owner_check: an Unknown attestation is unverified whatever its PID hint (B3).
        let live = |control| {
            OwnerJudgment::Bound(attestation(
                ProcessObservation::Live(Attested::Incarnation),
                control,
                Some(true),
            ))
        };
        let rows = [
            (
                OwnerJudgment::Bound(attestation(
                    ProcessObservation::Gone(Gone::Exited),
                    NotRecorded,
                    Some(false),
                )),
                json!(["unavailable", "owner_exited", [false, null, null]]),
            ),
            (
                OwnerJudgment::Bound(reused_attestation()),
                json!([
                    "unavailable",
                    "owner_identity_mismatch",
                    [true, false, null]
                ]),
            ),
            (
                live(Changed("owner TTY changed".into())),
                json!([
                    "unavailable",
                    "owner_identity_mismatch",
                    [true, false, null]
                ]),
            ),
            (
                live(Verified),
                json!(["available", "owner_identity_observed", [true, true, null]]),
            ),
            (
                live(NotRecorded),
                json!(["unknown", "owner_unverified", [true, null, null]]),
            ),
            (
                OwnerJudgment::Bound(attestation(
                    ProcessObservation::Live(Attested::PidOnly),
                    NotRecorded,
                    Some(true),
                )),
                json!(["unknown", "owner_unverified", [true, null, null]]),
            ),
            (
                live(Unknown("no TTY".into())),
                json!(["unknown", "owner_unverified", [true, null, "no TTY"]]),
            ),
            (
                OwnerJudgment::Bound(attestation(
                    ProcessObservation::Unknown("denied".into()),
                    NotRecorded,
                    Some(false),
                )),
                json!(["unknown", "owner_unverified", [false, null, "denied"]]),
            ),
            (
                OwnerJudgment::UnboundLegacy { alive: Some(false) },
                json!(["unknown", "owner_unbound_legacy", [false, null, null]]),
            ),
            (
                OwnerJudgment::Foreign,
                json!(["unavailable", "owner_session_mismatch", null]),
            ),
            (
                OwnerJudgment::Absent,
                json!(["unknown", "owner_unverified", null]),
            ),
            (
                OwnerJudgment::Unreadable("invalid JSON".into()),
                json!(["unknown", "owner_unreadable", null]),
            ),
        ];
        for (owner, expected) in rows {
            assert_eq!(owner_row(&owner), expected, "{owner:?}");
        }
        assert_eq!(observation.judgments.lifecycle, Lifecycle::Ready);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn correction_a3_intermediate_owner_doctor_distinguishes_changed_unknown_and_verified() {
        use crate::native::session::owner::tests::intermediate_attestation;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::native::session::Store::open_unchecked(dir.path());
        for (control, availability, reason) in [
            ("changed", "unavailable", "owner_identity_mismatch"),
            ("unknown", "unknown", "owner_unverified"),
            ("verified", "available", "owner_identity_observed"),
        ] {
            let evidence = bound_evidence(&store);
            let owner = evidence.judge_owner_with(
                "session-test",
                &judgment::Observers {
                    attest: &|_| intermediate_attestation(control),
                    pid_alive: &|_| panic!("a bound record is attested"),
                },
            );
            let row = owner_row(owner);
            assert_eq!(row[0], availability, "{control}");
            assert_eq!(row[1], reason, "{control}");
        }
    }

    #[test]
    fn r1_c6_reused_owner_is_unavailable_with_mismatch_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::native::session::Store::open_unchecked(dir.path());
        let evidence = bound_evidence(&store);
        let owner = evidence.judge_owner_with(
            "session-test",
            &judgment::Observers {
                attest: &|_| reused_attestation(),
                pid_alive: &|_| panic!("a bound record is attested"),
            },
        );
        let row = owner_row(owner);
        assert_eq!(row[0], "unavailable");
        assert_eq!(row[1], "owner_identity_mismatch");
    }
}
