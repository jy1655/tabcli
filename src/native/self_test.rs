use super::*;
use crate::native::session::{Reader, RecordStore, Store};
use agent_bridge::PUBLIC_COMMAND;
use serde_json::Value;

// Let each public command report its timeout before the outer deadline.
const COMMAND_MARGIN: Duration = Duration::from_secs(5);
// Each close and read-only confirmation gets its own fixed deadline.
const CLEANUP_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct Request {
    ask: AskRequest,
    isolated: bool,
}

pub(super) fn parse(args: &[String]) -> Result<NativeCommand> {
    let mut isolated = args.first().is_some_and(|arg| arg == "--isolated");
    let args = if isolated { &args[1..] } else { args };
    let (provider, options) = args
        .split_first()
        .context("self-test requires codex, claude, agy, or pi")?;
    // Reuse the new-session option contract, but keep the diagnostic prompt and lifecycle private.
    let mut ask_args = vec![provider.clone()];
    let mut index = 0;
    while index < options.len() {
        let option = &options[index];
        match option.as_str() {
            "--workspace" | "--terminal" | "--model" | "--effort" | "--timeout-secs" => {
                ask_args.push(option.clone());
                ask_args.push(option_value(options, &mut index, option)?.to_owned());
            }
            "--yolo" | "--json" => ask_args.push(option.clone()),
            "--isolated" => set_flag_once(&mut isolated, "--isolated")?,
            other => bail!("unknown self-test option: {other}"),
        }
        index += 1;
    }
    if !options.iter().any(|option| option == "--timeout-secs") {
        ask_args.extend(["--timeout-secs".to_owned(), "120".to_owned()]);
    }
    ask_args.extend(["--prompt".to_owned(), "self-test".to_owned()]);
    let NativeCommand::Ask(ask) = parse_ask(&ask_args)? else {
        unreachable!()
    };
    Ok(NativeCommand::SelfTest(Request { ask, isolated }))
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Passed,
    Failed,
    TimedOut,
    Unsupported,
    NotVerified,
}

#[derive(Debug, Serialize)]
struct Step {
    name: &'static str,
    outcome: Outcome,
    elapsed_ms: u128,
    request_address: Option<String>,
    event_address: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct CleanupSession {
    session: String,
    session_state: Option<String>,
    outcome: Outcome,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    bridge_version: &'static str,
    provider: String,
    provider_version: Option<String>,
    terminal: Option<String>,
    state_root: PathBuf,
    isolated: bool,
    marker: String,
    session: Option<String>,
    session_state: Option<String>,
    outcome: Outcome,
    elapsed_ms: u128,
    steps: Vec<Step>,
    cleanup_sessions: Vec<CleanupSession>,
}

struct Reply {
    ok: bool,
    value: Value,
    error: Option<String>,
}

// Public command execution and ownership discovery are the only runtime-facing operations.
trait Operations {
    fn call(&mut self, args: &[String], budget: Duration) -> Reply;
    fn private_sessions(&self) -> Result<Vec<String>>;
    fn owned_session(&self, workspace: &Path, title: &str) -> Result<Option<String>>;
    fn metadata(&self, session: &str) -> Result<(String, Option<String>)>;
}

struct Installed {
    executable: PathBuf,
    root: PathBuf,
    isolated: bool,
}

impl Installed {
    fn private_sessions(&self) -> Result<Vec<String>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_session_id(&name) && entry.file_type()?.is_dir() {
                sessions.push(name);
            }
        }
        sessions.sort();
        Ok(sessions)
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = crate::native::process_env::helper_command(&self.executable);
        command.args(args);
        command.env(STATE_DIR_ENV, &self.root);
        command
    }
}

impl Operations for Installed {
    fn call(&mut self, args: &[String], budget: Duration) -> Reply {
        let label = format!("self-test {} command did not return", args[0]);
        let output = checked_deadline_from(Instant::now(), budget)
            .and_then(|deadline| command_output_until(&mut self.command(args), deadline, &label));
        match output {
            Ok(output) => {
                let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
                let error = value["error"].as_str().map(str::to_owned).or_else(|| {
                    (!output.status.success())
                        .then(|| String::from_utf8_lossy(&output.stderr).trim().to_owned())
                });
                Reply {
                    ok: output.status.success() && value["ok"] == true,
                    value,
                    error,
                }
            }
            Err(error) => Reply {
                ok: false,
                value: Value::Null,
                error: Some(error.to_string()),
            },
        }
    }

    fn private_sessions(&self) -> Result<Vec<String>> {
        Installed::private_sessions(self)
    }

    fn owned_session(&self, workspace: &Path, title: &str) -> Result<Option<String>> {
        if self.isolated {
            return Ok(self.private_sessions()?.into_iter().next());
        }
        let NativeCommand::Sessions(request) = parse_sessions(&arguments(&[
            "--workspace",
            &workspace.to_string_lossy(),
            "--json",
        ]))?
        else {
            unreachable!()
        };
        let sessions = sessions_in_read_only(&self.root, &request)?;
        session_with_title(&sessions, title)
    }

    fn metadata(&self, session: &str) -> Result<(String, Option<String>)> {
        let directory = Reader::session_directory_in(&self.root, session)?;
        let manifest = Reader::open_unchecked(&directory).manifest()?;
        let terminal = Reader::open_unchecked(&directory)
            .terminal()
            .or_else(|_| Reader::open_unchecked(&directory).terminal_closed())
            .ok()
            .map(|terminal| terminal.kind.as_str().to_owned());
        Ok((manifest.provider_version, terminal))
    }
}

fn session_with_title(sessions: &[Value], title: &str) -> Result<Option<String>> {
    let matches: Vec<_> = sessions
        .iter()
        .filter(|session| session["title"] == title)
        .collect();
    if matches.len() != 1 {
        bail!(
            "found {} sessions with the exact self-test title; cleanup ownership is not verified",
            matches.len()
        )
    }
    let id = matches[0]["id"]
        .as_str()
        .context("matching session has no id")?;
    require_valid_session_id(id)?;
    Ok(Some(id.to_owned()))
}

fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn failure(reply: &Reply) -> (Outcome, String) {
    let reason = reply
        .error
        .clone()
        .unwrap_or_else(|| "command did not provide a verified outcome".to_owned());
    let lower = reason.to_ascii_lowercase();
    let outcome = if lower.contains("could not confirm delivery")
        || lower.contains("delivery uncertain")
        || lower.contains("delivery-uncertain")
        || lower.contains("may have been accepted")
    {
        Outcome::NotVerified
    } else if reply.value["timed_out"] == true || lower.contains("timed out") {
        Outcome::TimedOut
    } else if lower.contains("not yet supported")
        || lower.contains("is not available on windows")
        || lower.contains("only available on windows")
        || lower.contains("unsupported")
        || lower.contains("is too old: found")
        || (lower.contains("unavailable") && lower.contains("no terminal input was sent"))
    {
        Outcome::Unsupported
    } else if reply.value["request_state"] == "pending" || reply.error.is_none() {
        Outcome::NotVerified
    } else {
        Outcome::Failed
    };
    (outcome, reason)
}

fn step(name: &'static str, start: Instant, reply: &Reply) -> Step {
    let (outcome, reason) = if reply.ok {
        (Outcome::Passed, None)
    } else {
        let (outcome, reason) = failure(reply);
        (outcome, Some(reason))
    };
    let address = |field: &str| {
        Some(format!(
            "{}/{}",
            reply.value["session"].as_str()?,
            reply.value[field].as_str()?
        ))
    };
    Step {
        name,
        outcome,
        elapsed_ms: start.elapsed().as_millis(),
        request_address: address("request_id"),
        event_address: address("event_id"),
        reason,
    }
}

fn reject(step: &mut Step, outcome: Outcome, reason: &str) {
    step.outcome = outcome;
    step.reason = Some(reason.to_owned());
}

fn verify_result(
    step: &mut Step,
    reply: &Reply,
    session: &str,
    request: &str,
    marker: &str,
    previous_event: Option<&str>,
) {
    if step.outcome != Outcome::Passed {
        return;
    }
    if reply.value["session"] != session || reply.value["request_id"] != request {
        reject(
            step,
            Outcome::Failed,
            "result belongs to a different session or request",
        );
    } else if reply.value["request_state"] == "failed" {
        reject(
            step,
            Outcome::Failed,
            reply.value["error"]
                .as_str()
                .unwrap_or("provider request failed"),
        );
    } else if reply.value["request_state"] != "completed"
        || reply.value["event_id"].as_str().is_none()
    {
        reject(
            step,
            Outcome::NotVerified,
            "request has no verified completed event",
        );
    } else if reply.value["result"].as_str() != Some(marker) {
        reject(
            step,
            Outcome::Failed,
            "result does not exactly match the marker",
        );
    } else if previous_event.is_some() && reply.value["event_id"].as_str() == previous_event {
        reject(
            step,
            Outcome::Failed,
            "follow-up reused the initial event identity",
        );
    }
}

fn orchestrate(
    request: &Request,
    operations: &mut impl Operations,
    root: PathBuf,
    marker: String,
) -> Report {
    let started = Instant::now();
    let ask = &request.ask;
    let timeout = ask.timeout.as_secs().to_string();
    let prompt = format!(
        "No tool, command, or file is needed. Reply with exactly this marker and nothing else: {marker}"
    );
    let mut report = Report {
        schema_version: 1,
        bridge_version: env!("CARGO_PKG_VERSION"),
        provider: ask.provider.as_str().to_owned(),
        provider_version: None,
        terminal: ask.terminal.map(|kind| kind.as_str().to_owned()),
        state_root: root,
        isolated: request.isolated,
        marker,
        session: None,
        session_state: None,
        outcome: Outcome::NotVerified,
        elapsed_ms: 0,
        steps: Vec::new(),
        cleanup_sessions: Vec::new(),
    };
    let title = format!("Agent Bridge self-test {}", report.marker);
    let mut args = arguments(&["ask", ask.provider.as_str(), "--workspace"]);
    args.push(ask.workspace.to_string_lossy().into_owned());
    args.extend(arguments(&[
        "--prompt",
        &prompt,
        "--timeout-secs",
        &timeout,
        "--detach",
        "--json",
        "--title",
        &title,
    ]));
    for (option, value) in [
        ("--model", ask.model.as_deref()),
        ("--effort", ask.effort.as_deref()),
    ] {
        if let Some(value) = value {
            args.extend(arguments(&[option, value]));
        }
    }
    if let Some(kind) = ask.terminal {
        args.extend(arguments(&["--terminal", kind.as_str()]));
    }
    if ask.yolo {
        args.push("--yolo".to_owned());
    }
    let start = Instant::now();
    let initial = operations.call(&args, ask.timeout.saturating_add(COMMAND_MARGIN));
    let mut initial_step = step("ask", start, &initial);
    report.session = initial.value["session"].as_str().map(str::to_owned);
    let ownership = match report.session.as_deref() {
        Some(id) => require_valid_session_id(id).map(|()| Some(id.to_owned())),
        _ => operations.owned_session(&ask.workspace, &title),
    };
    let ownership_error = match ownership {
        Ok(Some(session)) => {
            report.session = Some(session);
            None
        }
        Ok(None) if request.isolated => {
            report.session = None;
            None
        }
        Ok(None) => Some("no session with the exact self-test title was found".to_owned()),
        Err(error) => Some(format!("cannot verify cleanup ownership: {error:#}")),
    };
    if initial_step.outcome == Outcome::Passed
        && (report.session.is_none()
            || initial.value["session"].as_str() != report.session.as_deref()
            || initial.value["request_id"].as_str().is_none())
    {
        reject(
            &mut initial_step,
            Outcome::NotVerified,
            "ask did not identify its owned session and request",
        );
    }
    if let Some(session) = &report.session
        && let Ok((version, terminal)) = operations.metadata(session)
    {
        report.provider_version = Some(version);
        if terminal.is_some() {
            report.terminal = terminal;
        }
    }
    report.steps.push(initial_step);
    let mut first_event = None;
    let mut current_request = initial.value["request_id"].as_str().map(str::to_owned);
    for name in ["initial_result", "tell", "follow_up_result"] {
        if report.steps.last().unwrap().outcome != Outcome::Passed {
            report.steps.push(Step {
                name,
                outcome: Outcome::NotVerified,
                elapsed_ms: 0,
                request_address: None,
                event_address: None,
                reason: Some("not run because a preceding step was not verified".to_owned()),
            });
            continue;
        }
        let session = report.session.as_deref().unwrap();
        let request_id = current_request.as_deref().unwrap();
        let args = if name == "tell" {
            arguments(&[
                "tell",
                session,
                "--prompt",
                &prompt,
                "--timeout-secs",
                &timeout,
                "--detach",
                "--json",
            ])
        } else {
            arguments(&[
                "result",
                session,
                "--request",
                request_id,
                "--wait",
                "--timeout-secs",
                &timeout,
                "--json",
            ])
        };
        let start = Instant::now();
        let reply = operations.call(&args, ask.timeout.saturating_add(COMMAND_MARGIN));
        let mut current = step(name, start, &reply);
        if name == "tell" {
            if current.outcome == Outcome::Passed
                && (reply.value["session"] != session
                    || reply.value["request_id"].as_str().is_none()
                    || reply.value["request_id"] == initial.value["request_id"])
            {
                reject(
                    &mut current,
                    Outcome::Failed,
                    "tell did not provide a distinct request in the owned session",
                );
            }
            current_request = reply.value["request_id"].as_str().map(str::to_owned);
        } else {
            verify_result(
                &mut current,
                &reply,
                session,
                request_id,
                &report.marker,
                first_event.as_deref(),
            );
            if current.outcome == Outcome::TimedOut
                && let Some(detail) =
                    doctor::result_timeout_detail(ask.provider, session, request_id, |args| {
                        let diagnosis = operations.call(args, COMMAND_MARGIN);
                        diagnosis.ok.then_some(diagnosis.value)
                    })
            {
                current.reason = Some(format!(
                    "{}; {detail}",
                    current.reason.as_deref().unwrap_or("waiting timed out")
                ));
            }
            if name == "initial_result" {
                first_event = reply.value["event_id"].as_str().map(str::to_owned);
            }
        }
        report.steps.push(current);
    }
    let start = Instant::now();
    let mut cleanup = Step {
        name: "cleanup",
        outcome: Outcome::Passed,
        elapsed_ms: 0,
        request_address: None,
        event_address: None,
        reason: Some("no session was created".to_owned()),
    };
    let mut targets: Vec<String> = report
        .session
        .iter()
        .filter(|id| valid_session_id(id))
        .cloned()
        .collect();
    let mut discovery_error = ownership_error;
    if request.isolated {
        match operations.private_sessions() {
            Ok(sessions) => {
                for session in sessions {
                    if !targets.contains(&session) {
                        targets.push(session);
                    }
                }
            }
            Err(error) => {
                discovery_error = Some(format!("cannot discover all private sessions: {error:#}"))
            }
        }
    }
    for session in targets {
        let close_start = Instant::now();
        let reply = operations.call(
            &arguments(&["close-session", &session, "--explicit", "--json"]),
            CLEANUP_BUDGET,
        );
        let mut closed = step("cleanup", close_start, &reply);
        let observed =
            operations.call(&arguments(&["inspect", &session, "--json"]), CLEANUP_BUDGET);
        let state = observed.value["stored_state"].as_str().map(str::to_owned);
        if report.session.as_deref() == Some(&session) {
            report.session_state = state.clone();
        }
        if closed.outcome == Outcome::Passed {
            if !observed.ok {
                let (outcome, reason) = failure(&observed);
                reject(&mut closed, outcome, &reason);
            } else if reply.value["session"] != session
                || reply.value["closed"] != true
                || state.as_deref() != Some("closed")
            {
                reject(
                    &mut closed,
                    Outcome::NotVerified,
                    "close could not be confirmed; inspect the reported session in the reported state root",
                );
            } else if !observed.value["residual_surface"].is_null() {
                reject(
                    &mut closed,
                    Outcome::NotVerified,
                    "close succeeded but a residual surface is recorded",
                );
            }
        }
        if closed.outcome != Outcome::Passed {
            let reason = format!(
                "session {session}: surface cleanup not verified; {}; stored state={}; recorded error={:?}",
                closed
                    .reason
                    .as_deref()
                    .unwrap_or("close was not confirmed"),
                state.as_deref().unwrap_or("unknown"),
                observed.value["error"]
                    .as_str()
                    .unwrap_or("no recorded surface error"),
            );
            reject(&mut closed, Outcome::NotVerified, &reason);
        }
        if cleanup.outcome == Outcome::Passed {
            cleanup.outcome = closed.outcome;
            cleanup.reason = closed.reason.clone();
        }
        report.cleanup_sessions.push(CleanupSession {
            session,
            session_state: state,
            outcome: closed.outcome,
            reason: closed.reason,
        });
    }
    if let Some(reason) = discovery_error
        && cleanup.outcome == Outcome::Passed
    {
        reject(&mut cleanup, Outcome::NotVerified, &reason);
    }
    cleanup.elapsed_ms = start.elapsed().as_millis();
    report.steps.push(cleanup);
    report.outcome = report
        .steps
        .iter()
        .find(|step| step.outcome != Outcome::Passed)
        .map(|step| step.outcome)
        .unwrap_or(Outcome::Passed);
    report.elapsed_ms = started.elapsed().as_millis();
    report
}

pub(super) fn run(request: Request) -> Result<()> {
    let root = if request.isolated {
        let mut builder = tempfile::Builder::new();
        builder.prefix("agent-bridge-self-test-");
        let directory = match std::env::var_os(STATE_DIR_ENV) {
            Some(root) => {
                fs::create_dir_all(&root)?;
                builder.tempdir_in(root)?
            }
            None => builder.tempdir()?,
        };
        let root = directory.keep();
        RecordStore::at(&root).set_directory_private()?;
        root
    } else {
        Reader::state_root()?
    };
    let marker = format!(
        "AB_{}",
        Store::new_event_file_name()?.trim_end_matches(".json")
    );
    let mut installed = Installed {
        executable: std::env::current_exe()?,
        root: root.clone(),
        isolated: request.isolated,
    };
    let report = orchestrate(&request, &mut installed, root, marker);
    if request.ask.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{PUBLIC_COMMAND} {} self-test: {}",
            report.bridge_version, report.provider
        );
        println!(
            "mode: {}\nCLI: {}\nterminal: {}\nstate root: {}\nsession: {}\nstate: {}",
            if report.isolated {
                "isolated"
            } else {
                "ordinary state root"
            },
            terminal_safe_text(
                report.provider_version.as_deref().unwrap_or("not verified"),
                false
            ),
            report.terminal.as_deref().unwrap_or("not verified"),
            terminal_safe_text(&report.state_root.display().to_string(), false),
            report.session.as_deref().unwrap_or("none"),
            report.session_state.as_deref().unwrap_or("not verified")
        );
        for session in &report.cleanup_sessions {
            println!(
                "cleanup session {}: {} state={}{}",
                session.session,
                serde_json::to_value(session.outcome)?.as_str().unwrap(),
                session.session_state.as_deref().unwrap_or("not verified"),
                session
                    .reason
                    .as_ref()
                    .map(|reason| format!("; {}", terminal_safe_text(reason, true)))
                    .unwrap_or_default()
            );
        }
        for step in &report.steps {
            println!(
                "{}: {} ({} ms) request={} event={}{}",
                step.name,
                serde_json::to_value(step.outcome)?.as_str().unwrap(),
                step.elapsed_ms,
                step.request_address.as_deref().unwrap_or("none"),
                step.event_address.as_deref().unwrap_or("none"),
                step.reason
                    .as_ref()
                    .map(|reason| format!("; {}", terminal_safe_text(reason, true)))
                    .unwrap_or_default()
            );
        }
        println!(
            "outcome: {} ({} ms)",
            serde_json::to_value(report.outcome)?.as_str().unwrap(),
            report.elapsed_ms
        );
    }
    if report.outcome != Outcome::Passed {
        bail!("self-test did not pass; see the report for request outcomes and cleanup")
    }
    Ok(())
}

#[cfg(test)]
mod tests;
