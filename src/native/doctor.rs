//! Observations and advice only. No recovery, delivery, terminal control, or settings writes.
use super::query::observation::{Observation, SessionEvidence, SurfaceRecord};
use super::{
    Duration, FirstPartyCli, Instant, NativeCommand, OsString, Output, Path, Reader, Result,
    SESSION_DIR_ENV, SeekFrom, Serialize, SessionManifest, Stdio, bail, cli_version_is_supported,
    consent, fs, is_executable, option_value, provider, provider_process, query, reopen,
    require_valid_session_id, resolve_provider, set_flag_once, set_once, terminal,
    terminal_safe_text, thread, unix_ms,
};
use crate::native::session::SessionState;
use crate::native::{FromStr, Read, Seek, provider_version_command};
use agent_bridge::PUBLIC_COMMAND;
use serde_json::{Value, json};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PROBE_OUTPUT: u64 = 64 * 1024;

// A provider owns which observation can explain a timed-out result and whether it
// needs an explicit probe. The caller owns the bound on executing the command.
pub(super) struct ResultTimeoutDiagnostic {
    pub(super) probe: bool,
    pub(super) detail: for<'a> fn(&'a Value, &str) -> Option<&'a str>,
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
    report["checks"]
        .as_array()?
        .iter()
        .find_map(|check| (diagnostic.detail)(check, request_id).map(str::to_owned))
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
    use Availability::*;
    let started = unix_ms();
    let deadline = Instant::now() + PROBE_TIMEOUT;
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
                        let mut version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                        if version.is_empty() {
                            version = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        }
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
    }
    let report = json!({
        "schema_version": 1, "ok": true, "session": request.session,
        "provider": provider.map(|p| p.as_str()), "probe": request.probe,
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
    checks: &mut Vec<Check>,
    observations: &mut Value,
) -> (Option<SessionManifest>, SessionEvidence) {
    let mut evidence = None;
    let manifest = match Observation::read(reader) {
        Ok(observation) => {
            session_checks(&observation, checks, observations);
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
            reader.manifest().ok()
        }
    };
    let mut evidence = evidence.unwrap_or_else(|| SessionEvidence::read(reader));
    owner_check(&mut evidence, id, checks);
    terminal_check(&evidence, id, checks);
    (manifest, evidence)
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

fn owner_check(evidence: &mut SessionEvidence, id: &str, checks: &mut Vec<Check>) {
    use Availability::*;
    let owner = &evidence.owner;
    let (availability, reason, detail, evidence) = match owner {
        Ok(Some(owner)) if owner.managed_session_id.as_deref() == Some(id) => {
            let observed = evidence.observe_owner();
            let (availability, reason) = match (observed.process_alive, observed.identity_matches) {
                (Some(false), _) => (Unavailable, "owner_exited"),
                (_, Some(false)) => (Unavailable, "owner_identity_mismatch"),
                (Some(true), Some(true)) => (Available, "owner_identity_observed"),
                _ => (Unknown, "owner_unverified"),
            };
            (
                availability,
                reason,
                "Owner observation is separate from terminal surface or provider readiness."
                    .to_owned(),
                json!(observed),
            )
        }
        Ok(Some(owner)) if owner.managed_session_id.is_some() => (
            Unavailable,
            "owner_session_mismatch",
            "Owner record belongs to another managed session.".to_owned(),
            Value::Null,
        ),
        Ok(Some(_)) => (Unknown, "owner_unbound_legacy", "A legacy owner record has no managed-session binding; process observation is not proof of control authority.".to_owned(), json!(evidence.observe_owner())),
        Ok(None) => (
            Unknown,
            "owner_unverified",
            "No owner record bound to this managed session was observed.".to_owned(),
            Value::Null,
        ),
        Err(error) => (
            Unknown,
            "owner_unreadable",
            format!("{error:#}"),
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
    let mut command = if arguments == ["--version"] {
        let mut command = provider_version_command(executable)?;
        command.arg("--version");
        command
    } else {
        provider_process::command(
            executable,
            scratch.path(),
            arguments.iter().map(OsString::from).collect(),
        )?
    };
    command
        .current_dir(workspace.unwrap_or(scratch.path()))
        .env_remove(SESSION_DIR_ENV);
    provider::apply_environment_removals(&mut command, environment_removals);
    provider_process::configure_process_tree(&mut command);
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
