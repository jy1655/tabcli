//! Observations and advice only. No recovery, delivery, terminal control, or settings writes.
use super::query::observation::judgment::{BoundOwner, OwnerJudgment};
use super::query::observation::{Observation, SessionEvidence, SurfaceRecord};
use super::{
    Duration, FirstPartyCli, Instant, NativeCommand, OsString, Path, Reader, Result, Serialize,
    bail, cli_version_is_supported, consent, fs, is_executable, option_value, provider,
    provider_process, provider_version_text, query, reopen, resolve_provider, set_flag_once,
    set_once, terminal, terminal_safe_text, thread,
};
use crate::native::session::SessionState;
use crate::native::session::origin::{OriginObservation, SourceKind};
use crate::native::session::primitives::{SESSION_DIR_ENV, require_valid_session_id, unix_ms};
use crate::native::session::schema::SessionManifest;
use crate::native::{FromStr, Read, process_env};
use agent_bridge::PUBLIC_COMMAND;
use serde_json::{Value, json};
use std::io::{Seek, SeekFrom};
use std::process::{Output, Stdio};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT: u64 = 64 * 1024;

// A provider owns which observation can explain a timed-out result and whether it
// needs an explicit probe. The caller owns the bound on executing the command.
pub(super) struct ResultTimeoutDiagnostic {
    pub(super) probe: bool,
    pub(super) detail: for<'a> fn(&'a TimeoutCheck, &str) -> Option<&'a str>,
}

// A row from the child doctor's JSON, including sparse or malformed observations.
// Read each field independently so an unrelated row cannot hide a later diagnosis.
pub(super) struct TimeoutCheck {
    pub(super) id: Option<String>,
    pub(super) reason_code: Option<String>,
    pub(super) request_id: Option<String>,
    pub(super) detail: Option<String>,
}

impl TimeoutCheck {
    fn from_row(row: &Value) -> Self {
        Self {
            id: row["id"].as_str().map(str::to_owned),
            reason_code: row["reason_code"].as_str().map(str::to_owned),
            request_id: row["evidence"]["request_id"].as_str().map(str::to_owned),
            detail: row["detail"].as_str().map(str::to_owned),
        }
    }
}

pub(super) fn result_timeout_detail(
    provider: FirstPartyCli,
    session: &str,
    request_id: &str,
    mut observe: impl FnMut(&[String]) -> Option<Value>,
) -> Option<String> {
    let diagnostic = provider::result_timeout_diagnostic(provider)?;
    let mut args = vec!["doctor".to_owned(), session.to_owned()];
    if diagnostic.probe {
        args.push("--probe".to_owned());
    }
    args.push("--json".to_owned());
    let report = observe(&args)?;
    if report["session"] != session {
        return None;
    }
    report["checks"].as_array()?.iter().find_map(|row| {
        let check = TimeoutCheck::from_row(row);
        (diagnostic.detail)(&check, request_id).map(str::to_owned)
    })
}

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

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Availability {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Debug, Serialize)]
pub(super) struct Check {
    pub(super) id: &'static str,
    pub(super) availability: Availability,
    pub(super) reason_code: &'static str,
    observed_unix_ms: u128,
    detail: String,
    next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<Vec<String>>,
    evidence: Value,
}

impl Check {
    pub(super) fn new(
        id: &'static str,
        availability: Availability,
        reason_code: &'static str,
        detail: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Self {
            id,
            availability,
            reason_code,
            observed_unix_ms: unix_ms(),
            detail: detail.into(),
            next_action: next_action.into(),
            next_command: None,
            evidence: Value::Null,
        }
    }

    pub(super) fn evidence(mut self, evidence: Value) -> Self {
        self.evidence = evidence;
        self
    }

    fn command(mut self, arguments: Vec<String>) -> Self {
        self.next_command = Some(arguments);
        self
    }
}

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub(super) directory: Option<&'a Path>,
    pub(super) manifest: Option<&'a SessionManifest>,
    pub(super) executable: Option<&'a Path>,
    pub(super) current_version: Option<&'a str>,
    pub(super) workspace: &'a Path,
    pub(super) probe: bool,
    pub(super) deadline: Instant,
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
        "platform",
        if cfg!(any(target_os = "macos", windows)) {
            Available
        } else {
            Unavailable
        },
        if cfg!(any(target_os = "macos", windows)) {
            "platform_supported"
        } else {
            "platform_unsupported"
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
                    "session_records",
                    Unavailable,
                    "session_unavailable",
                    format!("{error:#}"),
                    "Check the explicit session id and state directory.",
                ));
                unknown_session_checks(id, "session_unavailable", &mut checks);
                cancel_check(Some("unreadable"), id, &mut checks);
                unobserved_origin_check(id, &mut checks);
                hold_check(
                    &Err(anyhow::anyhow!("session unavailable")),
                    false,
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
            Ok(kind) if kind.supported_on_this_platform() => {
                (Available, "terminal_selected", json!({"terminal": kind}))
            }
            Ok(kind) => (
                Unavailable,
                "terminal_unsupported",
                json!({"terminal": kind}),
            ),
            Err(error) => (
                Unavailable,
                "terminal_selection_failed",
                json!({"error": format!("{error:#}")}),
            ),
        };
        checks.push(Check::new("terminal_selection", availability, reason,
            "Default terminal selection from the launch adapter; no app was opened or controlled.",
            "An explicit ask --terminal can select another supported host; live surface availability has not been tested.").evidence(evidence));
        #[cfg(windows)]
        checks.push(match terminal::windows_powershell_executable() {
            Ok(path) => Check::new(
                "powershell",
                Available,
                "powershell_found",
                "PowerShell 7 executable was resolved.",
                "The managed console is created only by ask.",
            )
            .evidence(json!({"path": path})),
            Err(error) => Check::new(
                "powershell",
                Unavailable,
                "powershell_unavailable",
                format!("{error:#}"),
                "Install PowerShell 7 and check absolute PATH entries.",
            ),
        });
    }
    let manifest = directory.as_ref().and_then(|directory| {
        let (manifest, mut evidence) = record_checks(
            &Reader::open_unchecked(directory),
            request.session.as_deref().unwrap(),
            deadline,
            &mut checks,
            &mut observations,
        );
        if request.probe {
            surface_check(
                &mut evidence,
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
                "provider",
                Unknown,
                "provider_unrecognized",
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
                checks.push(Check::new("provider_executable", Available, "executable_found", "Provider executable found; presence alone does not establish runtime capability.", "Use --probe to read its current version.").evidence(json!({"path": path})));
                executable = Some(path);
            }
            Ok(path) if !path.is_absolute() => {
                checks.push(Check::new("provider_executable", Unknown, "executable_path_unverified", "The recorded executable path is relative; doctor does not substitute a PATH lookup for it.", "Inspect the original launch configuration.").evidence(json!({"path": path})));
            }
            outcome => {
                let detail = match outcome {
                    Ok(path) => format!(
                        "Provider executable is missing, not executable, or not absolute: {}",
                        path.display()
                    ),
                    Err(error) => format!("{error:#}"),
                };
                checks.push(Check::new("provider_executable", Unavailable, "executable_unavailable", detail, "Check the provider installation and PATH; existing sessions retain their recorded executable path."));
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
                                Ok(true) => (Available, "version_supported"),
                                Ok(false) => (Unavailable, "version_unsupported"),
                                Err(_) => (Unknown, "version_unrecognized"),
                            };
                        checks.push(Check::new("provider_version", availability, reason, format!("Current CLI version; required minimum {}", provider.minimum_version()), "CLI version is only one gate; inspect the provider-specific checks.").evidence(json!({"current_version": version})));
                        current_version = Some(version);
                    }
                    outcome => {
                        let detail = match outcome {
                            Ok(output) => format!("Version probe exited with {}", output.status),
                            Err(error) => format!("{error:#}"),
                        };
                        checks.push(Check::new("provider_version", Unknown, "version_probe_failed", detail, "Inspect the provider installation; a failed probe does not prove availability."));
                    }
                }
            } else {
                checks.push(Check::new(
                    "provider_version",
                    Unknown,
                    "executable_unavailable",
                    "No unambiguous executable is available for a version probe.",
                    "Inspect the provider_executable check.",
                ));
            }
        } else {
            checks.push(Check::new("provider_version", Unknown, "probe_not_requested", "Current CLI version was not queried. A launch-recorded version is historical evidence only.", "Add --probe for bounded local version checks; no model or message is sent.").evidence(json!({"minimum_version": provider.minimum_version().to_string()})));
        }
        let workspace = manifest
            .as_ref()
            .map(|m| m.workspace.clone())
            .map_or_else(std::env::current_dir, Ok)?;
        let (availability, reason) = match fs::metadata(&workspace) {
            Ok(metadata) if metadata.is_dir() => (Available, "workspace_present"),
            Ok(_) => (Unavailable, "workspace_not_directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Unavailable, "workspace_missing")
            }
            Err(_) => (Unknown, "workspace_unreadable"),
        };
        checks.push(Check::new("workspace", availability, reason, "Working directory observation; CLI version checks do not depend on this directory.", "Restore or inspect the recorded directory before workspace-relative operations.").evidence(json!({"path": workspace})));
        if let Some(directory) = &directory {
            let consent = consent::observe(directory);
            consent_state = consent["state"].clone();
            let availability = if consent["state"] == "verified" {
                Available
            } else {
                Unknown
            };
            checks.push(Check::new("workspace_consent", availability, "recorded_workspace_consent",
                "Launch-time workspace consent and its provider-owned source. This is historical evidence, not a new trust grant.",
                "Inspect workspace_consent; consent revoke disables sharing, and consent reset permits fresh provider evidence on a later ask.").evidence(consent));
        }
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
) -> (Option<SessionManifest>, SessionEvidence) {
    let mut evidence = None;
    let manifest = match Observation::read(reader) {
        Ok(observation) => {
            session_checks(&observation, checks, observations);
            cancel_check(observation.cancel.as_ref().map(|c| c.state), id, checks);
            let origin = observation.observe_origin(reader, Some(deadline));
            origin.add_fields(observations);
            origin_check(
                &origin,
                observation.records.status.state == SessionState::Closed,
                id,
                checks,
            );
            evidence = Some(observation.evidence);
            Some(observation.records.manifest)
        }
        Err(error) => {
            let reason = if error.is::<query::SnapshotBusy>() {
                "records_busy"
            } else {
                "records_unreadable"
            };
            checks.push(Check::new("session_records", Availability::Unknown, reason, format!("{error:#}"), "Inspect the records without deleting locks or resending a request; retry if a writer is active."));
            unknown_session_checks(id, reason, checks);
            let cancel = reader
                .lock_shared()
                .and_then(|lock| super::session::cancel::observe(reader, lock.is_some()));
            cancel_check(
                Some(match cancel {
                    Ok(None) => "absent",
                    Ok(Some(_)) => "requested",
                    Err(_) => "unreadable",
                }),
                id,
                checks,
            );
            unobserved_origin_check(id, checks);
            reader.manifest().ok()
        }
    };
    let evidence = evidence.unwrap_or_else(|| match reader.lock_shared() {
        Ok(lock) => SessionEvidence::read(
            reader,
            super::session::hold::observe(reader, lock.is_some()),
        ),
        Err(error) => SessionEvidence::read(reader, Err(error)),
    });
    owner_check(evidence.judge_owner(id), checks);
    terminal_check(&evidence, id, checks);
    hold_check(
        &evidence.held,
        evidence.hold_release_allowed
            && observations["stored_state"]
                .as_str()
                .is_some_and(|state| state != "closed"),
        id,
        checks,
    );
    (manifest, evidence)
}

fn cancel_check(state: Option<&str>, id: &str, checks: &mut Vec<Check>) {
    let (availability, reason, detail) = match state {
        None | Some("absent") => (
            Availability::Available,
            "cancel_absent",
            "No cancel request is recorded.",
        ),
        Some("unreadable") => (
            Availability::Unknown,
            "cancel_unreadable",
            "The cancel request cannot be read reliably; published results remain independent.",
        ),
        _ => (
            Availability::Available,
            "cancel_recorded",
            "A cancel request is recorded; only a correlated provider result confirms interruption.",
        ),
    };
    checks.push(Check::new(
        "cancel",
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
        (SourceKind::External, _) => Check::new(
            "origin",
            Availability::Available,
            "origin_external",
            "The initial request came from outside Agent Bridge; there is no managed origin to observe.",
            "No action is needed.",
        ),
        (_, Some(false)) => Check::new(
            "origin",
            Availability::Available,
            "origin_live",
            "The origin session's records and its attested owner show it has not ended.",
            "No action is needed.",
        ),
        (_, Some(true)) if session_closed => Check::new(
            "origin",
            Availability::Available,
            "origin_ended_session_closed",
            "The origin session has ended, and this session is already closed.",
            "No action is needed.",
        ),
        (_, Some(true)) => Check::new(
            "origin",
            Availability::Unavailable,
            "origin_ended",
            "The origin session has ended in its records; this session is not closed. This is not evidence of an open surface.",
            "If this session is no longer needed, close it explicitly; the origin observation grants no close authority.",
        )
        .command(vec![
            PUBLIC_COMMAND.to_owned(),
            "close-session".to_owned(),
            id.to_owned(),
            "--explicit".to_owned(),
        ]),
        (_, None) => Check::new(
            "origin",
            Availability::Unknown,
            "origin_unverified",
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

fn hold_check(held: &Result<bool>, release_allowed: bool, id: &str, checks: &mut Vec<Check>) {
    let check = match held {
        Ok(false) => Check::new(
            "hold",
            Availability::Available,
            "not_held",
            "No follow-up hold is recorded.",
            "No hold release is needed.",
        ),
        Ok(true) => Check::new(
            "hold",
            Availability::Unavailable,
            "session_held",
            "The user's hold refuses follow-ups whose delivery has not begun.",
            if release_allowed {
                format!("{PUBLIC_COMMAND} hold {id} --release")
            } else {
                "Preserve the hold record; inspect the session's recorded lifecycle before taking action.".to_owned()
            },
        ),
        Err(error) => Check::new(
            "hold",
            Availability::Unknown,
            "hold_unreadable",
            format!("{error:#}"),
            format!("{PUBLIC_COMMAND} inspect {id} --json"),
        ),
    };
    checks.push(check);
}

/// The origin check when no consistent snapshot of this session could be read.
fn unobserved_origin_check(id: &str, checks: &mut Vec<Check>) {
    checks.push(Check::new(
        "origin",
        Availability::Unknown,
        "origin_unverified",
        "No consistent session snapshot was available, so the origin was not observed.",
        format!("{PUBLIC_COMMAND} inspect {id} --json"),
    ));
}

fn unknown_session_checks(id: &str, reason: &'static str, checks: &mut Vec<Check>) {
    for check_id in ["session_state", "turn", "completion"] {
        checks.push(Check::new(check_id, Availability::Unknown, reason,
            "No consistent session snapshot was available; missing evidence is not a ready or completed state.",
            format!("{PUBLIC_COMMAND} result {id} --list --json")));
    }
}

fn session_checks(observation: &Observation, checks: &mut Vec<Check>, observations: &mut Value) {
    use Availability::*;
    let snapshot = &observation.records;
    let active = observation.active_request();
    let result_action = active.map_or_else(
        || {
            format!(
                "{PUBLIC_COMMAND} result {} --list --json",
                snapshot.manifest.id
            )
        },
        |r| {
            format!(
                "{PUBLIC_COMMAND} result {} --request {} --json",
                snapshot.manifest.id, r.request_id
            )
        },
    );
    let request_state = observation.judgments.active_state.as_ref().map(|state| {
        state
            .as_ref()
            .map(|state| state.as_str())
            .unwrap_or("unknown")
    });
    *observations = json!({"stored_state": snapshot.status.state, "generation": snapshot.status.generation,
        "updated_unix_ms": snapshot.status.updated_unix_ms, "session_error": snapshot.status.error,
        "active_request_id": active.map(|r| &r.request_id), "active_request_state": request_state,
        "recovery_required": snapshot.pending.is_some()});
    if snapshot.claim.is_some() && active.is_none() {
        checks.push(Check::new("active_request", Unknown, "active_request_unknown", "The active claim has no readable request receipt; its public request identity is unknown.", &result_action));
    }
    checks.push(Check::new(
        "session_records",
        Available,
        "records_read",
        "Observed stored state without recovery; this is not a live delivery check.",
        "Use inspect for recorded launch and result details.",
    ));
    checks.push(Check::new(
        "session_state",
        if snapshot.status.state == SessionState::Ready {
            Available
        } else {
            Unavailable
        },
        "stored_state",
        format!("Stored state: {}", snapshot.status.state),
        &result_action,
    ));
    let (availability, reason, detail) = match (&snapshot.claim, &snapshot.status.error) {
        (Some(_), Some(_))
            if snapshot.status.state == SessionState::Working && snapshot.pending.is_none() =>
        {
            (
                Unknown,
                "delivery_unconfirmed",
                "A turn remains claimed with an error. Delivery may have occurred; do not resend.",
            )
        }
        (Some(_), _) => (
            Unavailable,
            "turn_in_progress",
            "A turn is claimed; wait for the same request result instead of sending another turn.",
        ),
        (None, _) => (
            Available,
            "no_active_turn",
            "No active turn claim was observed.",
        ),
    };
    let launch_failure = &observation.judgments.launch_failure;
    let (availability, reason, detail) = if let Some((reason, detail)) = launch_failure {
        (Unavailable, *reason, detail.as_str())
    } else {
        (availability, reason, detail)
    };
    checks.push(Check::new(
        "turn",
        availability,
        reason,
        detail,
        &result_action,
    ));
    let mut completion = Check::new(
        "completion",
        if snapshot.pending.is_some() {
            Unavailable
        } else {
            Available
        },
        if snapshot.pending.is_some() {
            "recovery_required"
        } else {
            "no_pending_completion"
        },
        "Completion journal observation; doctor never publishes or repairs a result.",
        if snapshot.pending.is_some() {
            "The suggested sessions command changes state: it recovers completion and can close dead-owner sessions in that workspace. Run it explicitly, then query the same request."
        } else {
            "Retrieve recorded results with result."
        },
    );
    if snapshot.pending.is_some() {
        completion = completion.command(vec![
            PUBLIC_COMMAND.to_owned(),
            "sessions".to_owned(),
            "--workspace".to_owned(),
            snapshot.manifest.workspace.to_string_lossy().into_owned(),
            "--json".to_owned(),
        ]);
    }
    checks.push(completion);
    if snapshot.unreadable_requests > 0 || snapshot.request_index_error.is_some() {
        checks.push(Check::new("request_index", Unknown, "request_index_incomplete", "Some request references could not be read; absence is not proof that no request exists.", "Use an exact event id when known; preserve damaged request records.")
            .evidence(json!({"unreadable_requests": snapshot.unreadable_requests, "error": snapshot.request_index_error})));
    }
}

fn surface_check(
    evidence: &mut SessionEvidence,
    reader: &Reader,
    deadline: Instant,
    checks: &mut Vec<Check>,
) {
    use Availability::*;
    let result = evidence.observe_surface(reader, deadline);
    let (availability, reason, detail) = match result {
        Ok(true) => (Available, "terminal_surface_present", "Recorded terminal surface is present; this does not prove provider readiness or delivery.".to_owned()),
        Ok(false) => (Unavailable, "terminal_surface_missing", "Recorded terminal surface is absent; a provider process may still survive it.".to_owned()),
        Err(error) => (Unknown, "terminal_surface_unverified", format!("Surface presence could not be verified: {error:#}")),
    };
    checks.push(Check::new("terminal_surface", availability, reason, detail,
        "Inspect the launch log and exact request; this read-only probe does not release a claim or resend input."));
}

// Adapter fixtures exercise the production observation and doctor mapping together.
#[cfg(all(test, target_os = "macos"))]
pub(in crate::native) fn surface_check_for_test(
    evidence: &mut SessionEvidence,
    reader: &Reader,
) -> Check {
    let mut checks = Vec::new();
    surface_check(
        evidence,
        reader,
        Instant::now() + Duration::from_secs(2),
        &mut checks,
    );
    assert_eq!(checks.len(), 1);
    checks.remove(0)
}

fn owner_check(owner: &OwnerJudgment, checks: &mut Vec<Check>) {
    use Availability::*;
    let (availability, reason, detail, evidence) = match owner {
        OwnerJudgment::Bound(attestation) => {
            // An Unknown attestation is unverified whatever its PID hint (B3).
            let (availability, reason) = match BoundOwner::of(attestation) {
                BoundOwner::Exited => (Unavailable, "owner_exited"),
                BoundOwner::Reused | BoundOwner::ControlChanged => {
                    (Unavailable, "owner_identity_mismatch")
                }
                BoundOwner::Verified => (Available, "owner_identity_observed"),
                BoundOwner::Uncompared | BoundOwner::Unknown => (Unknown, "owner_unverified"),
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
            "owner_session_mismatch",
            "Owner record belongs to another managed session.".to_owned(),
            Value::Null,
        ),
        OwnerJudgment::UnboundLegacy { .. } => (Unknown, "owner_unbound_legacy", "A legacy owner record has no managed-session binding; process observation is not proof of control authority.".to_owned(), json!(owner.observation())),
        OwnerJudgment::Absent => (
            Unknown,
            "owner_unverified",
            "No owner record bound to this managed session was observed.".to_owned(),
            Value::Null,
        ),
        OwnerJudgment::Unreadable(error) => (
            Unknown,
            "owner_unreadable",
            error.clone(),
            Value::Null,
        ),
    };
    checks.push(Check::new("owner", availability, reason, detail, "Inspect the exact request result; do not infer delivery or resend from owner liveness.").evidence(evidence));
}

fn terminal_check(evidence: &SessionEvidence, id: &str, checks: &mut Vec<Check>) {
    use Availability::*;
    match &evidence.surface {
        SurfaceRecord::Closed(record) => match record {
            Ok(value) => {
                let consumed = value.get("consumed").and_then(Value::as_bool) == Some(true);
                checks.push(Check::new(
                    "terminal_record",
                    if consumed { Unavailable } else { Unknown },
                    if consumed {
                        "terminal_consumed"
                    } else {
                        "terminal_tombstone_unreadable"
                    },
                    "A terminal tombstone is recorded; a consumed handle cannot be reused.",
                    "Inspect retained results; doctor does not reopen or delete terminal records.",
                ));
            }
            Err(error) => {
                checks.push(Check::new(
                    "terminal_record",
                    Unknown,
                    "terminal_tombstone_unreadable",
                    format!("{error:#}"),
                    "Preserve the terminal records and inspect the close attempt.",
                ));
            }
        },
        SurfaceRecord::Closing(record) => match record {
            Ok(terminal) => {
                let bound = terminal.verify_managed_session(id).is_ok();
                checks.push(Check::new("terminal_record", if bound { Unavailable } else { Unknown },
                if bound { "terminal_close_in_progress" } else { "terminal_binding_unverified" },
                "A claimed close handle remains; the close may still be running or require explicit recovery.",
                "Inspect the prior close attempt. Finishing it requires an explicit close request and ownership verification."));
            }
            Err(error) => {
                checks.push(Check::new(
                    "terminal_record",
                    Unknown,
                    "terminal_record_unreadable",
                    format!("{error:#}"),
                    "Preserve and inspect the close handle.",
                ));
            }
        },
        SurfaceRecord::Active(record) => {
            let (availability, reason, detail, evidence) = match record {
                Ok(Some(terminal)) => {
                    let supported = terminal.kind.supported_on_this_platform();
                    let (availability, reason) = if !supported {
                        (Unavailable, "terminal_unsupported")
                    } else if terminal.verify_managed_session(id).is_err() || terminal.id.is_empty()
                    {
                        (Unknown, "terminal_binding_unverified")
                    } else {
                        (Available, "terminal_record_found")
                    };
                    (
                        availability,
                        reason,
                        "Stored terminal metadata only; no live surface control was attempted."
                            .to_owned(),
                        json!({"terminal": terminal.kind}),
                    )
                }
                Ok(None) => (
                    Unavailable,
                    "terminal_record_missing",
                    "No active terminal handle is recorded.".to_owned(),
                    Value::Null,
                ),
                Err(error) => (
                    Unknown,
                    "terminal_record_unreadable",
                    format!("{error:#}"),
                    Value::Null,
                ),
            };
            checks.push(Check::new("terminal_record", availability, reason, detail, "Only tell/close revalidate their target surface; this observation grants no control authority.").evidence(evidence));
        }
    }
}

// Probe only explicitly requested local CLI information. Scratch files (including Windows
// batch forwarders) live outside the session; contain descendants and cap time/output.
fn probe_command(
    executable: &Path,
    arguments: &[&str],
    workspace: Option<&Path>,
    scratch: &Path,
    environment_removals: &[&str],
) -> Result<std::process::Command> {
    let mut command = if arguments == ["--version"] {
        let mut command = process_env::provider_version_command(executable, environment_removals)?;
        command.arg("--version");
        command
    } else {
        process_env::provider_command(
            executable,
            scratch,
            arguments.iter().map(OsString::from).collect(),
            environment_removals,
        )?
    };
    command
        .current_dir(workspace.unwrap_or(scratch))
        .env_remove(SESSION_DIR_ENV);
    provider_process::configure_process_tree(&mut command);
    Ok(command)
}

pub(super) fn probe(
    executable: &Path,
    arguments: &[&str],
    workspace: Option<&Path>,
    environment_removals: &[&str],
    deadline: Instant,
) -> Result<Output> {
    if Instant::now() >= deadline {
        bail!("diagnostic probe deadline exhausted");
    }
    let scratch = tempfile::tempdir()?;
    let mut command = probe_command(
        executable,
        arguments,
        workspace,
        scratch.path(),
        environment_removals,
    )?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    if Instant::now() >= deadline {
        bail!("diagnostic probe deadline exhausted before launch");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let tree = match provider_process::ProviderProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let outcome = (|| {
        tree.resume(&child)?;
        let status = loop {
            if stdout
                .metadata()?
                .len()
                .saturating_add(stderr.metadata()?.len())
                > MAX_PROBE_OUTPUT
            {
                bail!("diagnostic probe output limit exceeded");
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                bail!("diagnostic probe timed out");
            }
            thread::sleep(Duration::from_millis(20));
        };
        tree.terminate();
        stdout.seek(SeekFrom::Start(0))?;
        stderr.seek(SeekFrom::Start(0))?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout.take(MAX_PROBE_OUTPUT + 1).read_to_end(&mut out)?;
        stderr.take(MAX_PROBE_OUTPUT + 1).read_to_end(&mut err)?;
        if out.len().saturating_add(err.len()) as u64 > MAX_PROBE_OUTPUT {
            bail!("diagnostic probe output limit exceeded");
        }
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    })();
    tree.terminate();
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn doctor_commands_keep_only_adapter_removals_and_the_session_directory_removal() {
        for provider in agent_bridge::supported_clis() {
            let removals = provider::probe_environment_removals(*provider);
            for args in [vec!["--version"], vec!["diagnostic"]] {
                let command =
                    probe_command(Path::new("unused"), &args, None, Path::new("."), removals)
                        .unwrap();
                crate::native::process_env::tests::assert_provider_environment(
                    &command,
                    removals,
                    &[(SESSION_DIR_ENV, None)],
                );
            }
        }
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn local_probes_bound_output_and_stop_a_hung_cli() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("provider");
        fs::write(&executable, "#!/bin/sh\nprintf '%066000d' 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let error = probe(
            &executable,
            &["--version"],
            Some(directory.path()),
            &[],
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("output limit"), "{error:#}");
        fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let error = probe(
            &executable,
            &["--version"],
            Some(directory.path()),
            &[],
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn agy_timeout_predicate_requires_only_reason_and_matching_request() {
        let diagnostic = provider::result_timeout_diagnostic(FirstPartyCli::Agy).unwrap();
        assert!(!diagnostic.probe);
        let row = json!({
            "reason_code": "agy_tool_confirmation_observed",
            "evidence": {"request_id": "request-current"},
            "detail": "approval needed"
        });
        let check = TimeoutCheck::from_row(&row);
        assert_eq!(
            (diagnostic.detail)(&check, "request-current"),
            Some("approval needed")
        );
        assert_eq!((diagnostic.detail)(&check, "request-other"), None);
        for (field, value) in [
            ("reason_code", json!("other")),
            ("reason_code", json!(17)),
            ("evidence", json!({"request_id": 17})),
            ("evidence", Value::Null),
            ("detail", json!(17)),
        ] {
            let mut changed = row.clone();
            changed[field] = value;
            assert_eq!(
                (diagnostic.detail)(&TimeoutCheck::from_row(&changed), "request-current"),
                None
            );
        }
        let mut unrelated = row;
        unrelated["id"] = json!("unrelated");
        unrelated["availability"] = json!("available");
        assert_eq!(
            (diagnostic.detail)(&TimeoutCheck::from_row(&unrelated), "request-current"),
            Some("approval needed")
        );
    }

    #[test]
    fn pi_timeout_predicate_requires_only_credentials_id() {
        let diagnostic = provider::result_timeout_diagnostic(FirstPartyCli::Pi).unwrap();
        assert!(diagnostic.probe);
        for availability in ["available", "unavailable", "unknown"] {
            let row = json!({
                "id": "pi_provider_credentials", "detail": "credentials observation",
                "availability": availability, "reason_code": "unrelated",
                "evidence": {"request_id": "request-other"}
            });
            let check = TimeoutCheck::from_row(&row);
            assert_eq!(
                (diagnostic.detail)(&check, "request-current"),
                Some("credentials observation")
            );
        }
        for row in [
            json!({"id": "other", "detail": "ignored"}),
            json!({"id": 17, "detail": "ignored"}),
            json!({"id": "pi_provider_credentials", "detail": false}),
            json!({"detail": "ignored"}),
        ] {
            assert_eq!(
                (diagnostic.detail)(&TimeoutCheck::from_row(&row), "request-current"),
                None
            );
        }
    }

    #[test]
    fn timeout_rows_preserve_sparse_matches_and_first_matching_detail() {
        for (provider, row) in [
            (
                FirstPartyCli::Agy,
                json!({"reason_code": "agy_tool_confirmation_observed", "evidence": {"request_id": "request-current"}, "detail": "first"}),
            ),
            (
                FirstPartyCli::Pi,
                json!({"id": "pi_provider_credentials", "detail": "first"}),
            ),
        ] {
            let mut missing_detail = row.clone();
            missing_detail["detail"] = json!(3);
            let mut later = row.clone();
            later["detail"] = json!("second");
            assert_eq!(
                result_timeout_detail(provider, "session-owned", "request-current", |_| Some(
                    json!({
                        "session": "session-owned",
                        "checks": [null, 7, [], {}, missing_detail, row, later]
                    })
                )),
                Some("first".to_owned())
            );
        }
    }

    #[test]
    fn codex_and_claude_timeout_diagnostics_do_not_observe() {
        for provider in [FirstPartyCli::Codex, FirstPartyCli::Claude] {
            assert!(provider::result_timeout_diagnostic(provider).is_none());
            assert_eq!(
                result_timeout_detail(provider, "session-owned", "request-current", |_| panic!(
                    "no diagnostic may run"
                )),
                None
            );
        }
    }
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

    #[test]
    fn owner_check_maps_each_owner_judgment() {
        use ControlAttestation::*;
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
            // B3: a false PID hint beside an Unknown attestation is not an exit.
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
