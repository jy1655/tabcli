use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter, ResumeContext, ResumePlan, ResumedSessionContext,
};
#[cfg(test)]
use crate::native::session::SessionState;
use crate::native::session::turn;
use crate::native::session::{Reader, RecordStore, Store};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::super::terminal;

pub(super) static ADAPTER: PiAdapter = PiAdapter;

pub(super) struct PiAdapter;

const HOOK_FAILURE_FILE: &str = "pi-hook-failure.json";
const PENDING_TURN_FILE: &str = "pi-pending-turn.json";
#[cfg(any(windows, test))]
const STARTUP_READY_FILE: &str = "pi-startup-ready.json";

#[cfg(any(windows, test))]
#[derive(Deserialize, Serialize)]
struct StartupReady {
    schema: u32,
    claim_token: String,
    session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct PendingPiTurn {
    schema: u32,
    claim_token: String,
    marker: String,
}

impl PendingPiTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-pi-turn:{claim_token} -->"),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct HookFailureSignal {
    error: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    #[serde(default)]
    claim_token: Option<String>,
}

// Pi 0.84.4 keeps only transcript files under ~/.pi; no lock, pid, or registry identifies
// a live writer, and a transcript-based heuristic is not ownership evidence. Replace this
// refusal when Pi exposes a live-session registry or a held session lock.
const PI_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Pi exposes no verifiable ownership evidence for a session (no lock, pid, or registry under ~/.pi identifies a live writer)";

impl NativeProviderAdapter for PiAdapter {
    fn workspace_trust_key(&self, _screen: &str, _workspace: &Path) -> Option<terminal::DialogKey> {
        None
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        homes: &super::super::consent::Homes,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let Some(text) = consent::read_store(&homes.pi)? else {
            return Ok(Trust::Absent);
        };
        let config: serde_json::Value = serde_json::from_str(&text)?;
        let entries = config
            .as_object()
            .context("Pi trust store is not an object")?;
        let key = consent::native_key(workspace)?;
        match entries.get(&key) {
            Some(serde_json::Value::Bool(true)) => Ok(Trust::Trusted(Evidence {
                provider: "pi".into(),
                store: homes.pi.clone(),
                key,
            })),
            Some(serde_json::Value::Bool(false)) => Ok(Trust::Declined),
            None => {
                // A parent's positive decision is not exact-workspace consent; a
                // nearer negative decision still prevents overriding Pi's refusal.
                for parent in workspace.ancestors().skip(1) {
                    match entries.get(&consent::native_key(parent)?) {
                        Some(serde_json::Value::Bool(false)) => return Ok(Trust::Declined),
                        Some(serde_json::Value::Bool(true)) => break,
                        Some(_) => bail!("unknown Pi ancestor trust value"),
                        None => {}
                    }
                }
                Ok(Trust::Absent)
            }
            _ => bail!("unknown Pi trust value"),
        }
    }

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // Pi derives no session identity from the caller's environment.
        &[]
    }

    fn diagnose(
        &self,
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        use super::super::doctor::{Availability::Unknown, Check};
        vec![
            Check::new(
                "pi_follow_up",
                Unknown,
                "pi_terminal_fallback",
                "Pi owns terminal-paste follow-up and a session completion extension. No verified first-party external input path into a running interactive session is integrated.",
                "Inspect the managed owner and terminal. Live input was not tested; replace this fallback when Pi provides the required native path.",
            ),
            credential_diagnosis(context, |path, args, workspace, deadline| {
                super::super::doctor::probe(path, args, Some(workspace), &[], deadline)
            }),
        ]
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = turn::current_claim_token(&Reader::open_unchecked(context.directory))?
            .context("Pi launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let extension_path = context.directory.join("pi-agent-bridge.js");
        RecordStore::at(&extension_path).write_private(bridge_extension().as_bytes())?;
        let mut arguments = vec![
            OsString::from("--extension"),
            extension_path.into_os_string(),
            OsString::from("--name"),
            OsString::from(context.title),
        ];
        if super::super::consent::authorized(context.directory, FirstPartyCli::Pi)? {
            // Pi documents --approve as a one-run project-trust override. It is
            // not a tool permission flag and does not persist to trust.json.
            arguments.push(OsString::from("--approve"));
            super::super::consent::applied(context.directory, "pi-approve-once")?;
        }
        if !cfg!(windows) {
            arguments.push(OsString::from(correlated_prompt(context.prompt, &pending)));
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            // Replace the local lifecycle extension when Pi exposes a
            // first-party external completion callback with turn identity.
            completion_monitor: CompletionMonitor::PiHookFailure,
            environment_removals: &[],
        })
    }

    fn verify_reopen_available(&self, _provider_session_id: &str) -> Result<()> {
        bail!("{PI_REOPEN_UNSUPPORTED}")
    }

    fn prepare_resume(&self, _context: ResumeContext<'_>) -> Result<ResumePlan> {
        bail!("{PI_REOPEN_UNSUPPORTED}")
    }

    fn other_resumed_conversation_holders(
        &self,
        _context: ResumedSessionContext<'_>,
    ) -> Result<Vec<u32>> {
        bail!("{PI_REOPEN_UNSUPPORTED}")
    }

    fn initial_prompt_transport(&self) -> InitialPromptTransport {
        if cfg!(windows) {
            InitialPromptTransport::TerminalPasteAfterLaunch
        } else {
            InitialPromptTransport::ProviderArgument
        }
    }

    fn initial_prompt_ready_delay(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn send_initial_prompt(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        #[cfg(windows)]
        {
            let directory = session
                .managed_session_id
                .as_deref()
                .context("Pi initial input has no managed session identity")
                .and_then(Reader::session_directory)
                .map_err(terminal::TerminalSendFailure::not_sent)?;
            send_initial_prompt_after_startup(&directory, deadline, || {
                terminal::send_file(session, prompt_path, deadline)
            })
        }
        #[cfg(not(windows))]
        terminal::send_file(session, prompt_path, deadline)
    }

    fn terminal_initial_prompt(&self, directory: &Path, prompt: &str) -> Result<String> {
        let pending = read_pending_turn(directory)?
            .context("Pi initial turn correlation state is missing")?;
        terminal_correlated_prompt(prompt, &pending, cfg!(windows))
    }

    #[cfg(any(windows, test))]
    fn terminal_submit_count(&self) -> usize {
        1
    }

    fn follow_up_transport(&self) -> FollowUpTransport {
        // Replace this fallback when Pi exposes a verified first-party input
        // path into an already-running interactive session.
        FollowUpTransport::TerminalPasteFallback
    }

    fn new_cross_session_turn_id(&self) -> Result<String> {
        bail!("Pi does not support provider cross-session turns")
    }

    fn send_cross_session_message(
        &self,
        _context: CrossSessionMessageContext<'_>,
    ) -> CrossSessionMessageResult {
        Err(CrossSessionMessageFailure::not_sent(anyhow::anyhow!(
            "Pi does not support provider cross-session messages"
        )))
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        let Some(pending) = read_pending_turn(directory)? else {
            return Ok(());
        };
        if pi_string(payload, "agent_bridge_claim_token") != Some(pending.claim_token.as_str()) {
            return Ok(());
        }
        if payload
            .get("agent_bridge_prompt_correlated")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Ok(());
        }
        let provider_session_id = pi_owned_string(payload, "session_id");
        let turn_id = pi_owned_string(payload, "turn_id");
        if let Some(error) = pi_string(payload, "agent_bridge_error")
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            return record_correlated_failure(
                directory,
                error,
                provider_session_id,
                turn_id,
                &pending,
            );
        }
        let raw_message = pi_string(payload, "last_assistant_message")
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .context("Pi hook payload has no assistant result")?;
        let Ok(message) = correlated_response(raw_message, &pending) else {
            return Ok(());
        };
        turn::Report::for_claim(
            &Store::open_unchecked(directory),
            FirstPartyCli::Pi,
            Some(&pending.claim_token),
        )
        .complete(message, provider_session_id, turn_id)
        .context("failed to record the correlated Pi result")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Pi does not expose Agent Bridge provider controls")
    }

    fn send_terminal_follow_up(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        terminal::send_file(session, prompt_path, deadline)
    }

    fn prepare_terminal_follow_up(
        &self,
        directory: &Path,
        prompt: &str,
        claim_token: &str,
    ) -> Result<String> {
        let pending = install_pending_turn(directory, claim_token)?;
        terminal_correlated_prompt(prompt, &pending, cfg!(windows))
    }

    fn cancel_terminal_follow_up(&self, directory: &Path, claim_token: &str) -> Result<()> {
        cancel_pending_turn(directory, claim_token)
    }
}

// This observation changes nothing in turn correlation. Remove it when Pi emits an
// official prompt-rejection or completion signal identifying the submitted request
// before the agent turn starts. Configuration is not authentication acceptance.
fn credential_diagnosis(
    context: super::super::doctor::Context<'_>,
    mut probe: impl FnMut(&Path, &[&str], &Path, Instant) -> Result<std::process::Output>,
) -> super::super::doctor::Check {
    let unknown = |reason| credential_check(None, "unknown", reason, None, None);
    let Some(model) = context.manifest.and_then(|m| m.model.as_deref()) else {
        return unknown(
            "No model is known; a session with an explicit provider/model is required.",
        );
    };
    let Some(provider) = model_provider(model) else {
        return unknown("Provider not derivable from this model; use an explicit provider/model.");
    };
    if !context.probe {
        return credential_check(
            Some(&provider),
            "unknown",
            "Credential probe not requested; use doctor SESSION --probe.",
            None,
            None,
        );
    }
    let Some(executable) = context.executable else {
        return credential_check(
            Some(&provider),
            "unknown",
            "Pi command is missing or unavailable.",
            None,
            None,
        );
    };
    // Pi 1.0.0's resolver can prefer a raw model id on another provider even when
    // the prefix is known. Let Pi confirm the prefix, without refreshing or a turn;
    // never duplicate its catalog or infer a provider from an unqualified name.
    // Both commands share this budget, inside self-test's five-second doctor budget.
    let deadline = context
        .deadline
        .min(Instant::now() + Duration::from_secs(3));
    let resolved = probe(
        executable,
        &["auth", "check", "--model", model, "--no-refresh", "--json"],
        context.workspace,
        deadline,
    );
    let resolution = parse_credentials(&provider, resolved);
    if resolution.availability == super::super::doctor::Availability::Unknown {
        return resolution;
    }
    parse_credentials(
        &provider,
        probe(
            executable,
            &[
                "auth",
                "check",
                "--provider",
                &provider,
                "--no-refresh",
                "--json",
            ],
            context.workspace,
            deadline,
        ),
    )
}

fn model_provider(model: &str) -> Option<String> {
    let (provider, model) = model.split_once('/')?;
    if provider.is_empty()
        || model.is_empty()
        || model.chars().any(char::is_whitespace)
        || !provider
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(provider.to_ascii_lowercase())
}

fn credential_check(
    provider: Option<&str>,
    status: &str,
    reason: &str,
    auth_type: Option<&str>,
    exit_code: Option<i32>,
) -> super::super::doctor::Check {
    use super::super::doctor::{Availability, Check};
    let (availability, code) = match status {
        "ready" => (Availability::Available, "pi_provider_credentials_ready"),
        "not_ready" => (
            Availability::Unavailable,
            "pi_provider_credentials_not_ready",
        ),
        _ => (Availability::Unknown, "pi_provider_credentials_unknown"),
    };
    Check::new("pi_provider_credentials", availability, code,
        format!("Pi provider credentials {status}{}: {reason}", provider.map(|p| format!(" for {p}")).unwrap_or_default()),
        "Inspect Pi's authentication setup. This observation does not fail, cancel, or resend a request.")
        .evidence(serde_json::json!({"status": status, "provider": provider, "authType": auth_type, "exit_code": exit_code}))
}

fn parse_credentials(
    provider: &str,
    output: Result<std::process::Output>,
) -> super::super::doctor::Check {
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            // Never echo command output or arbitrary error strings that may contain secrets.
            let reason = if error.to_string().contains("timed out")
                || error.to_string().contains("deadline")
            {
                "Credential check timed out."
            } else {
                "Credential check could not run; Pi command missing or unavailable."
            };
            return credential_check(Some(provider), "unknown", reason, None, None);
        }
    };
    let exit = output.status.code();
    #[derive(Deserialize)]
    struct AuthStatus {
        status: String,
        provider: String,
        #[serde(rename = "authType")]
        auth_type: Option<String>,
        reason: Option<String>,
    }
    let parsed = serde_json::from_slice::<AuthStatus>(&output.stdout);
    let Ok(value) = parsed else {
        return credential_check(
            Some(provider),
            "unknown",
            "Credential check returned malformed or unsupported JSON.",
            None,
            exit,
        );
    };
    if value.provider != provider {
        return credential_check(
            Some(provider),
            "unknown",
            "Provider not derivable: Pi did not confirm the model's provider.",
            None,
            exit,
        );
    }
    let (status, reason, auth_type) = match (
        value.status.as_str(),
        value.reason.as_deref(),
        value.auth_type.as_deref(),
        exit,
    ) {
        // Pi 1.0.0 dist/cli/auth-check.js:24-30 gates ready on a checkAuth result,
        // then copies auth.type as metadata; dist/main.js:162 derives exit from status.
        // pi-ai/dist/models.js:263-294 delegates API-key checks and returns their result.
        // Although auth/types.d.ts:94-98 currently lists two types, the label does not
        // gate readiness. Preserve it as evidence rather than imposing our own gate.
        ("ready", _, kind, Some(0)) => (
            "ready",
            "Credentials are configured, not verified valid or accepted by the provider.",
            kind,
        ),
        ("not_ready", Some("credentials_not_configured"), None, Some(1)) => (
            "not_ready",
            "Pi has no credentials configured for this provider.",
            None,
        ),
        ("not_ready", Some("provider_not_found"), None, Some(1)) => (
            "unknown",
            "Provider not derivable: Pi does not recognize this provider (provider_not_found).",
            None,
        ),
        ("invalid", Some("invalid_state"), None, Some(2)) => (
            "unknown",
            "Pi reports invalid authentication state (invalid_state).",
            None,
        ),
        _ => (
            "unknown",
            "Credential check returned an unsupported status or exit code.",
            None,
        ),
    };
    credential_check(Some(provider), status, reason, auth_type, exit)
}

#[cfg(any(windows, test))]
fn send_initial_prompt_after_startup(
    directory: &Path,
    deadline: Instant,
    send: impl FnOnce() -> terminal::TerminalSendResult,
) -> terminal::TerminalSendResult {
    // Pi's session_start(startup) follows resolution of project_trust. A fixed delay
    // cannot prove that its trust dialog has gone away: Enter would choose a button.
    // This is readiness, not consent; Pi can reach session_start after declining trust.
    let wait = || -> Result<()> {
        let pending = read_pending_turn(directory)?.context("Pi initial turn is missing")?;
        loop {
            if Instant::now() >= deadline {
                bail!(
                    "Pi startup has not confirmed that project trust was resolved; no initial console input was sent. Resolve the prompt in the managed terminal, then close and start a new Bridge session if this request timed out"
                );
            }
            if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
                != Some(pending.claim_token.as_str())
            {
                bail!("Pi initial turn is no longer claimed; no initial console input was sent");
            }
            if let Some(text) = Reader::open_unchecked(directory)
                .private(STARTUP_READY_FILE)
                .text()?
            {
                let ready: StartupReady =
                    serde_json::from_str(&text).context("invalid Pi startup receipt")?;
                if ready.schema != 1
                    || ready.claim_token != pending.claim_token
                    || ready.session_id.trim().is_empty()
                {
                    bail!("Pi startup receipt does not identify this initial turn");
                }
                return Ok(());
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }
    };
    wait().map_err(terminal::TerminalSendFailure::not_sent)?;
    send()
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if claim_token.is_empty()
        || claim_token.len() > 160
        || !claim_token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid Pi turn claim token")
    }
    Ok(())
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingPiTurn> {
    let pending = PendingPiTurn::new(claim_token)?;
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .write_json(&pending)?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingPiTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
    else {
        return Ok(None);
    };
    let pending: PendingPiTurn =
        serde_json::from_str(&text).context("failed to parse the pending Pi turn")?;
    let expected = PendingPiTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Pi turn correlation state")
    }
    Ok(Some(pending))
}

fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .remove()
}

fn correlated_prompt(prompt: &str, pending: &PendingPiTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Pi turn protocol]\nCorrelation marker for the Agent Bridge extension:\n{}",
        pending.marker
    )
}

fn terminal_correlated_prompt(
    prompt: &str,
    pending: &PendingPiTurn,
    windows: bool,
) -> Result<String> {
    if !windows {
        return Ok(correlated_prompt(prompt, pending));
    }
    let encoded = serde_json::to_string(prompt)?;
    Ok(format!(
        "[Agent Bridge Pi Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs, and complete it as one turn. Request JSON: {encoded} Correlation marker for the Agent Bridge extension: {}",
        pending.marker
    ))
}

fn correlated_response<'a>(message: &'a str, pending: &PendingPiTurn) -> Result<&'a str> {
    let message = message.trim_end();
    let body = message
        .strip_suffix(&pending.marker)
        .unwrap_or(message)
        .trim_end();
    if body.is_empty() {
        bail!("Pi correlated response contained no assistant text")
    }
    Ok(body)
}

fn record_correlated_failure(
    directory: &Path,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    pending: &PendingPiTurn,
) -> Result<()> {
    turn::Report::for_claim(
        &Store::open_unchecked(directory),
        FirstPartyCli::Pi,
        Some(&pending.claim_token),
    )
    .fail(error, provider_session_id, turn_id)
    .context("failed to record the correlated Pi failure")
}

fn pi_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn pi_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    pi_string(payload, key).map(str::to_owned)
}

pub(super) struct PiFailureMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl PiFailureMonitor {
    pub(super) fn start(directory: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-pi-failure-monitor".to_owned())
            .spawn(move || {
                let result = monitor_hook_failures(&directory, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = turn::Report::monitor_failure(
                        &Store::open_unchecked(&error_directory),
                        FirstPartyCli::Pi,
                        &format!("Pi result recovery monitor failed: {error:#}"),
                    );
                }
                result
            })
            .context("failed to start Pi result recovery monitor")?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub(super) fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .context("Pi result recovery monitor handle was already consumed")?
            .join()
            .map_err(|_| anyhow::anyhow!("Pi result recovery monitor panicked"))??;
        Ok(())
    }
}

impl Drop for PiFailureMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn monitor_hook_failures(directory: &Path, stop: &AtomicBool) -> Result<()> {
    loop {
        consume_hook_failure(directory)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn consume_hook_failure(directory: &Path) -> Result<bool> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(HOOK_FAILURE_FILE)
        .text()?
    else {
        return Ok(false);
    };
    let signal: HookFailureSignal =
        serde_json::from_str(&text).context("invalid Pi hook failure recovery signal")?;
    let error = signal.error.trim();
    if error.is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    Store::open_unchecked(directory)
        .private(HOOK_FAILURE_FILE)
        .remove()
        .context("failed to consume Pi hook failure recovery signal")?;
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(true);
    };
    if signal.claim_token.as_deref() != Some(pending.claim_token.as_str()) {
        return Ok(true);
    }
    record_correlated_failure(
        directory,
        error,
        signal.provider_session_id,
        signal.turn_id,
        &pending,
    )?;
    Ok(true)
}

fn bridge_extension() -> &'static str {
    r#"import { spawnSync } from "node:child_process";
import { readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function assistantText(message) {
  if (!Array.isArray(message.content)) return undefined;
  const text = message.content
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim();
  return text || undefined;
}

function lastAssistantOutcome(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "stop") {
      const detail = typeof message.errorMessage === "string" && message.errorMessage.trim()
        ? `: ${message.errorMessage.trim()}`
        : "";
      return { agent_bridge_error: `Pi turn ended with ${message.stopReason ?? "an unknown state"}${detail}` };
    }
    const text = assistantText(message);
    if (text) return { last_assistant_message: text };
    return { agent_bridge_error: "Pi settled without assistant text." };
  }
  return { agent_bridge_error: "Pi settled without an assistant result." };
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function deliverResult(executable, payload) {
  if (!executable) {
    return { ok: false, detail: "Agent Bridge executable is unavailable" };
  }
  let detail = "native hook failed";
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const result = spawnSync(executable, ["native-hook", "pi"], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "pipe"],
      timeout: 10000,
    });
    if (!result.error && result.status === 0) return { ok: true };
    const stderr = typeof result.stderr === "string" ? result.stderr.trim() : "";
    detail = result.error?.message || stderr || `native hook exited with status ${result.status}`;
    if (attempt < 2) await wait(100 * (attempt + 1));
  }
  return { ok: false, detail: detail.slice(0, 1024) };
}

function persistHookFailure(payload, detail) {
  const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
  if (!directory) return false;
  const target = join(directory, "pi-hook-failure.json");
  const temporary = join(
    directory,
    `.pi-hook-failure-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`,
  );
  const signal = {
    error: `Pi result delivery failed: ${detail}`,
    provider_session_id: payload.session_id,
    turn_id: payload.turn_id,
    claim_token: payload.agent_bridge_claim_token,
  };
  try {
    writeFileSync(temporary, JSON.stringify(signal), { encoding: "utf8", flag: "wx", mode: 0o600 });
    renameSync(temporary, target);
    return true;
  } catch {
    try { unlinkSync(temporary); } catch {}
    return false;
  }
}

export default function (pi) {
  let pending;
  let undelivered = false;
  let activeClaimToken;
  let activePromptCorrelated = false;

  function readActiveClaimToken() {
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    if (!directory) return undefined;
    try {
      const token = readFileSync(join(directory, "turn.claim"), "utf8").trim();
      return token || undefined;
    } catch {
      return undefined;
    }
  }

  // Official session_start follows project_trust resolution, including a user's decline.
  // Never return a trust decision here or persist one in Pi's provider-owned store.
  pi.on("session_start", (event, ctx) => {
    if (event.reason !== "startup") return;
    const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
    const claimToken = readActiveClaimToken();
    if (!directory || !claimToken) return;
    const temporary = join(directory, `.pi-startup-${process.pid}-${Date.now()}.tmp`);
    try {
      writeFileSync(temporary, JSON.stringify({
        schema: 1,
        claim_token: claimToken,
        session_id: ctx.sessionManager.getSessionId(),
      }), { encoding: "utf8", flag: "wx", mode: 0o600 });
      renameSync(temporary, join(directory, "pi-startup-ready.json"));
    } catch {
      try { unlinkSync(temporary); } catch {}
      if (process.platform === "win32") {
        ctx.ui.notify("Agent Bridge could not confirm Pi startup; the initial console input will remain withheld.", "warning");
      }
    }
  });

  pi.on("before_agent_start", (event) => {
    activeClaimToken = readActiveClaimToken();
    activePromptCorrelated = Boolean(
      activeClaimToken
      && event.prompt.includes(`<!-- agent-bridge-pi-turn:${activeClaimToken} -->`),
    );
  });

  pi.on("agent_start", (_event, ctx) => {
    if (undelivered && pending) {
      if (!persistHookFailure(pending, "a prior result remained undelivered")) {
        ctx.ui.notify("Agent Bridge still cannot recover the previous Pi result.", "warning");
        return;
      }
      undelivered = false;
    }
    pending = undefined;
  });

  pi.on("agent_end", (event, ctx) => {
    if (undelivered) return;
    if (!activePromptCorrelated || !activeClaimToken) {
      pending = undefined;
      return;
    }
    pending = {
      ...lastAssistantOutcome(event.messages),
      session_id: ctx.sessionManager.getSessionId(),
      turn_id: ctx.sessionManager.getLeafId() ?? undefined,
      agent_bridge_claim_token: activeClaimToken,
      agent_bridge_prompt_correlated: true,
    };
  });

  pi.on("agent_settled", async (_event, ctx) => {
    if (!pending) return;
    const payload = pending;
    const executable = process.env.AGENT_BRIDGE_EXECUTABLE;
    const delivery = await deliverResult(executable, payload);
    if (!delivery.ok) {
      if (persistHookFailure(payload, delivery.detail)) {
        pending = undefined;
        ctx.ui.notify("Agent Bridge marked this undelivered Pi result as failed.", "warning");
        return;
      }
      undelivered = true;
      ctx.ui.notify("Agent Bridge could not record or recover this Pi result.", "warning");
      return;
    }
    pending = undefined;
  });
}
"#
}

#[cfg(test)]
mod tests {
    fn auth_output(text: &str, exit: i32) -> Result<std::process::Output> {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit as u32)
        };
        Ok(std::process::Output {
            status,
            stdout: text.as_bytes().to_vec(),
            stderr: b"sensitive stderr must not be reported".to_vec(),
        })
    }

    #[test]
    fn credential_status_shapes_are_bounded_to_known_fields_and_exit_codes() {
        // Pi 1.0.0, 2026-10-07: ready OAuth=0, missing credentials=1,
        // unknown provider=1. invalid_state=2 is from Pi's auth-check source.
        for (body, exit, expected) in [
            (
                r#"{"status":"ready","provider":"openai","authType":"oauth"}"#,
                0,
                "ready",
            ),
            (
                r#"{"status":"ready","provider":"openai","authType":"api_key","token":"secret"}"#,
                0,
                "ready",
            ),
            (
                r#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
                1,
                "not_ready",
            ),
            (
                r#"{"status":"not_ready","provider":"openai","reason":"provider_not_found"}"#,
                1,
                "unknown",
            ),
            (
                r#"{"status":"invalid","provider":"openai","reason":"invalid_state"}"#,
                2,
                "unknown",
            ),
            (
                r#"{"status":"ready","provider":"other","authType":"oauth"}"#,
                0,
                "unknown",
            ),
            (
                r#"{"status":"ready","provider":"openai","authType":"oauth"}"#,
                1,
                "unknown",
            ),
            (
                r#"{"status":"ready","provider":"openai","authType":"future_scheme"}"#,
                0,
                "ready",
            ),
            ("secret malformed JSON", 0, "unknown"),
            ("{}", 0, "unknown"),
        ] {
            let check =
                serde_json::to_value(parse_credentials("openai", auth_output(body, exit))).unwrap();
            assert_eq!(check["evidence"]["status"], expected, "{body}");
            assert_eq!(check["evidence"]["exit_code"], exit);
            assert!(!check.to_string().contains("secret"));
            assert!(!check.to_string().contains("sensitive"));
        }
        for (error, detail) in [
            ("command missing", "could not run"),
            ("diagnostic probe timed out", "timed out"),
            ("diagnostic probe deadline exhausted", "timed out"),
        ] {
            let check =
                serde_json::to_value(parse_credentials("openai", Err(anyhow::anyhow!(error))))
                    .unwrap();
            assert_eq!(check["evidence"]["status"], "unknown");
            assert!(check["detail"].as_str().unwrap().contains(detail));
        }
    }

    #[test]
    fn ready_credentials_preserve_auth_type_as_evidence_only() {
        use super::super::super::doctor::Availability;
        for auth_type in [Some("future_scheme"), None] {
            let output = serde_json::json!({
                "status": "ready", "provider": "openai", "authType": auth_type
            });
            let check = parse_credentials("openai", auth_output(&output.to_string(), 0));
            assert_eq!(check.availability, Availability::Available);
            assert_eq!(check.reason_code, "pi_provider_credentials_ready");
            let check = serde_json::to_value(check).unwrap();
            assert_eq!(check["evidence"]["authType"], serde_json::json!(auth_type));
            assert_eq!(check["evidence"]["status"], "ready");
        }
    }

    #[test]
    fn model_provider_requires_a_qualified_id_without_guessing() {
        for (model, expected) in [
            ("openai-codex/gpt-5.6-sol", Some("openai-codex")),
            ("OpenAI/gpt-4o-mini", Some("openai")),
            ("openrouter/openai/gpt-4o", Some("openrouter")),
            ("openai/custom:high", Some("openai")),
            ("gpt-4o", None),
            ("Fable", None),
            ("", None),
            ("/model", None),
            ("openai/", None),
            ("openai /model", None),
            ("openai/a b", None),
        ] {
            assert_eq!(model_provider(model).as_deref(), expected);
        }
    }

    #[test]
    fn credential_doctor_requires_session_model_probe_and_pi_resolution() {
        use super::super::super::{SessionManifest, doctor};
        let mut manifest = SessionManifest {
            schema: 1,
            id: "test".into(),
            provider: "pi".into(),
            provider_path: "pi".into(),
            provider_version: "1.0.0".into(),
            workspace: ".".into(),
            title: "test".into(),
            model: Some("openai/gpt-4o-mini".into()),
            effort: None,
            yolo: false,
            created_unix_ms: 1,
        };
        let base = doctor::Context {
            directory: None,
            manifest: None,
            executable: Some(Path::new("pi")),
            current_version: None,
            workspace: Path::new("."),
            probe: true,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        let check =
            credential_diagnosis(base, |_, _, _, _| panic!("provider-only must not run Pi"));
        let check = serde_json::to_value(check).unwrap();
        assert_eq!(check["id"], "pi_provider_credentials");
        assert_eq!(check["evidence"]["status"], "unknown");
        assert!(
            check["detail"]
                .as_str()
                .unwrap()
                .contains("No model is known")
        );
        for model in [None, Some("unqualified".into())] {
            manifest.model = model;
            credential_diagnosis(
                doctor::Context {
                    manifest: Some(&manifest),
                    ..base
                },
                |_, _, _, _| panic!("no derivable provider"),
            );
        }
        manifest.model = Some("openai/gpt-4o-mini".into());
        for (probe, executable) in [(false, base.executable), (true, None)] {
            let check = credential_diagnosis(
                doctor::Context {
                    manifest: Some(&manifest),
                    probe,
                    executable,
                    ..base
                },
                |_, _, _, _| panic!("must not run"),
            );
            assert_eq!(check.availability, doctor::Availability::Unknown);
        }
        for (resolved, credentials_missing) in
            [("openai", false), ("other", false), ("openai", true)]
        {
            let mut calls = Vec::new();
            let mut deadlines = Vec::new();
            let check = credential_diagnosis(
                doctor::Context {
                    manifest: Some(&manifest),
                    ..base
                },
                |_, args, _, deadline| {
                    calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                    deadlines.push(deadline);
                    if credentials_missing && calls.len() == 2 {
                        auth_output(
                            r#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
                            1,
                        )
                    } else {
                        auth_output(
                            &format!(
                                r#"{{"status":"ready","provider":"{resolved}","authType":"oauth"}}"#
                            ),
                            0,
                        )
                    }
                },
            );
            let expected = if credentials_missing {
                doctor::Availability::Unavailable
            } else if resolved == "openai" {
                doctor::Availability::Available
            } else {
                doctor::Availability::Unknown
            };
            assert_eq!(check.availability, expected);
            assert_eq!(
                calls[0],
                [
                    "auth",
                    "check",
                    "--model",
                    "openai/gpt-4o-mini",
                    "--no-refresh",
                    "--json"
                ]
            );
            if resolved == "openai" {
                assert_eq!(calls.len(), 2);
                assert_eq!(
                    calls[1],
                    [
                        "auth",
                        "check",
                        "--provider",
                        "openai",
                        "--no-refresh",
                        "--json"
                    ]
                );
                assert_eq!(deadlines[0], deadlines[1]);
            } else {
                assert_eq!(calls.len(), 1);
            }
            if credentials_missing {
                assert_eq!(check.id, "pi_provider_credentials");
                assert_eq!(check.reason_code, "pi_provider_credentials_not_ready");
                let check = serde_json::to_value(check).unwrap();
                assert_eq!(
                    check["detail"],
                    "Pi provider credentials not_ready for openai: Pi has no credentials configured for this provider."
                );
                assert_eq!(check["evidence"]["status"], "not_ready");
                assert_eq!(check["evidence"]["exit_code"], 1);
            }
        }
    }

    #[test]
    fn workspace_trust_requires_exact_positive_but_respects_parent_decline() {
        use super::super::super::consent::{self, Trust};
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let homes = consent::fixture_homes(tmp.path());
        let key = consent::native_key(&workspace).unwrap();
        super::super::super::write_json_atomic(&homes.pi, &serde_json::json!({key.clone():true}))
            .unwrap();
        assert!(matches!(
            ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
            Trust::Trusted(_)
        ));
        assert_eq!(
            ADAPTER
                .workspace_trust(&workspace.join("child"), &homes)
                .unwrap(),
            Trust::Absent
        );
        super::super::super::write_json_atomic(&homes.pi, &serde_json::json!({key:false})).unwrap();
        assert_eq!(
            ADAPTER
                .workspace_trust(&workspace.join("child"), &homes)
                .unwrap(),
            Trust::Declined
        );
        std::fs::write(&homes.pi, b"[]").unwrap();
        assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
    }

    #[test]
    fn diagnostics_describe_pi_owned_fallback_without_claiming_live_delivery() {
        use super::super::super::doctor::{Availability, Context};
        use super::NativeProviderAdapter;
        let checks = super::ADAPTER.diagnose(Context {
            directory: None,
            manifest: None,
            executable: None,
            current_version: None,
            workspace: std::path::Path::new("."),
            probe: false,
            deadline: std::time::Instant::now(),
        });
        assert_eq!(checks[0].reason_code, "pi_terminal_fallback");
        assert_eq!(checks[0].availability, Availability::Unknown);
        assert_eq!(checks[1].id, "pi_provider_credentials");
        assert_eq!(checks[1].availability, Availability::Unknown);
    }

    use std::fs;

    use super::super::super::{
        SessionEvent, SessionStatus, TURN_CLAIM_FILE, acquire_turn_claim, event_paths, read_json,
        update_status, wait_for_event, write_json_atomic,
    };
    use super::*;

    fn claim_pending_turn(directory: &Path) -> PendingPiTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        install_pending_turn(directory, &token).unwrap()
    }

    fn marked(message: &str, pending: &PendingPiTurn) -> String {
        format!("{message}\n{}", pending.marker)
    }

    #[test]
    fn unresolved_project_trust_never_receives_the_initial_console_paste() {
        let directory = tempfile::tempdir().unwrap();
        claim_pending_turn(directory.path());
        let sent = std::cell::Cell::new(false);
        let result = send_initial_prompt_after_startup(directory.path(), Instant::now(), || {
            sent.set(true);
            Ok(())
        });
        assert!(
            !sent.get(),
            "initial input could press Enter on the unresolved trust dialog"
        );
        let failure = result.unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
    }

    #[test]
    fn startup_receipt_allows_one_paste_and_preserves_the_transport_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let pending = claim_pending_turn(directory.path());
        write_json_atomic(
            &directory.path().join(STARTUP_READY_FILE),
            &StartupReady {
                schema: 1,
                claim_token: pending.claim_token,
                session_id: "pi-native-session".to_owned(),
            },
        )
        .unwrap();
        let sends = std::cell::Cell::new(0);
        let failure = send_initial_prompt_after_startup(
            directory.path(),
            Instant::now() + Duration::from_secs(1),
            || {
                sends.set(sends.get() + 1);
                Err(terminal::TerminalSendFailure::delivery_uncertain(
                    anyhow::anyhow!("transport uncertain"),
                ))
            },
        )
        .unwrap_err();
        assert_eq!(sends.get(), 1);
        assert!(failure.delivery_may_have_occurred());
        assert!(
            failure
                .into_error()
                .to_string()
                .contains("transport uncertain")
        );
    }

    #[test]
    fn stale_malformed_or_cancelled_startup_never_pastes() {
        let directory = tempfile::tempdir().unwrap();
        let pending = claim_pending_turn(directory.path());
        let valid = serde_json::json!({"schema": 1, "claim_token": pending.claim_token, "session_id": "native"});
        for receipt in [
            "not-json".to_owned(),
            serde_json::json!({"schema": 1, "claim_token": "1-2-3", "session_id": "native"}).to_string(),
            serde_json::json!({"schema": 2, "claim_token": pending.claim_token, "session_id": "native"}).to_string(),
            serde_json::json!({"schema": 1, "claim_token": pending.claim_token, "session_id": ""}).to_string(),
        ] {
            fs::write(directory.path().join(STARTUP_READY_FILE), receipt).unwrap();
            let failure = send_initial_prompt_after_startup(directory.path(), Instant::now() + Duration::from_secs(1), || panic!("unexpected paste")).unwrap_err();
            assert!(!failure.delivery_may_have_occurred());
        }
        fs::write(directory.path().join(STARTUP_READY_FILE), valid.to_string()).unwrap();
        fs::remove_file(directory.path().join(TURN_CLAIM_FILE)).unwrap();
        let failure = send_initial_prompt_after_startup(
            directory.path(),
            Instant::now() + Duration::from_secs(1),
            || panic!("cancelled turn pasted"),
        )
        .unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
    }

    #[test]
    fn windows_console_prompt_preserves_multiline_input_without_raw_submission_keys() {
        let pending = PendingPiTurn::new("1-2-3").unwrap();
        let prompt = "first line\nsecond\tcolumn";
        let framed = terminal_correlated_prompt(prompt, &pending, true).unwrap();

        assert!(
            framed
                .chars()
                .all(|character| !matches!(character, '\r' | '\n' | '\t'))
        );
        assert!(framed.contains(&serde_json::to_string(prompt).unwrap()));
        assert!(framed.contains(&pending.marker));
    }

    #[test]
    fn extension_reports_only_settled_results_without_changing_tool_policy() {
        let extension = bridge_extension();

        assert!(extension.contains("agent_start"));
        assert!(extension.contains("before_agent_start"));
        assert!(extension.contains("agent_end"));
        assert!(extension.contains("agent_settled"));
        assert!(extension.contains("stopReason"));
        assert!(extension.contains("agent_bridge_error"));
        assert!(extension.contains("agent_bridge_claim_token"));
        assert!(extension.contains("agent_bridge_prompt_correlated"));
        assert!(extension.contains("event.prompt.includes"));
        assert!(extension.contains("turn.claim"));
        assert!(extension.contains(HOOK_FAILURE_FILE));
        assert!(extension.contains("renameSync"));
        assert!(extension.contains("native-hook\", \"pi"));
        assert!(extension.contains("timeout: 10000"));
        assert!(!extension.contains("tool_call"));
        assert!(!extension.contains("--approve"));
    }

    #[test]
    fn pi_hook_owns_the_extension_payload_schema() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "session_id": "pi-session",
            "turn_id": "pi-turn",
            "last_assistant_message": marked("pi result", &pending),
            "agent_bridge_claim_token": pending.claim_token,
            "agent_bridge_prompt_correlated": true,
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "pi result");
        assert_eq!(event.provider_session_id.as_deref(), Some("pi-session"));
        assert_eq!(event.turn_id.as_deref(), Some("pi-turn"));
        assert!(directory.path().join(PENDING_TURN_FILE).is_file());
    }

    #[test]
    fn pi_hook_preserves_an_exact_response_when_the_input_was_correlated() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "session_id": "pi-session",
            "turn_id": "pi-turn",
            "last_assistant_message": "EXACT_RESPONSE",
            "agent_bridge_claim_token": pending.claim_token,
            "agent_bridge_prompt_correlated": true,
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "EXACT_RESPONSE");
    }

    #[test]
    fn pi_hook_rejects_a_matching_claim_without_input_correlation() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "session_id": "pi-session",
            "turn_id": "manual-turn",
            "last_assistant_message": "manual response",
            "agent_bridge_claim_token": pending.claim_token,
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        assert!(event_paths(directory.path()).unwrap().is_empty());
        assert!(directory.path().join(TURN_CLAIM_FILE).is_file());
    }

    #[test]
    fn hook_transport_failure_signal_recovers_the_bridge_turn() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        write_json_atomic(
            &directory.path().join(HOOK_FAILURE_FILE),
            &HookFailureSignal {
                error: "native hook exited with status 1".to_owned(),
                provider_session_id: Some("provider-session".to_owned()),
                turn_id: Some("provider-turn".to_owned()),
                claim_token: Some(pending.claim_token),
            },
        )
        .unwrap();

        assert!(consume_hook_failure(directory.path()).unwrap());
        assert!(!directory.path().join(TURN_CLAIM_FILE).exists());
        assert!(!directory.path().join(HOOK_FAILURE_FILE).exists());
        let error = wait_for_event(directory.path(), 0, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error:#}").contains("native hook exited with status 1"));
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
    }

    #[test]
    fn pi_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "session_id": "pi-session",
                    "turn_id": "pi-turn-1",
                    "last_assistant_message": marked("first result", &initial_pending),
                    "agent_bridge_claim_token": initial_pending.claim_token,
                    "agent_bridge_prompt_correlated": true,
                }),
            )
            .unwrap();
        let _later_pending = claim_pending_turn(directory.path());
        update_status(directory.path(), SessionState::Claimed, None, None).unwrap();
        update_status(directory.path(), SessionState::Working, None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "session_id": "pi-session",
                    "turn_id": "delayed-old-turn-with-a-new-id",
                    "last_assistant_message": "delayed old result",
                    "agent_bridge_claim_token": initial_pending.claim_token,
                    "agent_bridge_prompt_correlated": true,
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[test]
    fn reopen_is_refused_with_the_adapters_own_reason() {
        let error = ADAPTER
            .verify_reopen_available("5e58ec26-0000-4000-8000-000000000000")
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Pi"),
            "{error}"
        );
        let directory = tempfile::tempdir().unwrap();
        let error = ADAPTER
            .prepare_resume(ResumeContext {
                bridge_executable: std::path::Path::new("/opt/agent-bridge"),
                directory: directory.path(),
                provider_session_id: "5e58ec26-0000-4000-8000-000000000000",
            })
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Pi"),
            "{error}"
        );
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }
}
