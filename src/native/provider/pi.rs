use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter,
};
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
    time::Duration,
};

use super::super::terminal;

pub(super) static ADAPTER: PiAdapter = PiAdapter;

pub(super) struct PiAdapter;

const HOOK_FAILURE_FILE: &str = "pi-hook-failure.json";
const PENDING_TURN_FILE: &str = "pi-pending-turn.json";
const PENDING_TURN_CONSUMING_FILE: &str = "pi-pending-turn.consuming.json";

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

impl NativeProviderAdapter for PiAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = super::super::current_turn_claim_token(context.directory)?
            .context("Pi launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let extension_path = context.directory.join("pi-agent-bridge.js");
        super::super::write_private(&extension_path, bridge_extension().as_bytes())?;
        let mut arguments = vec![
            OsString::from("--extension"),
            extension_path.into_os_string(),
            OsString::from("--name"),
            OsString::from(context.title),
        ];
        if !cfg!(windows) {
            arguments.push(OsString::from(correlated_prompt(context.prompt, &pending)));
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            // Replace the local lifecycle extension when Pi exposes a
            // first-party external completion callback with turn identity.
            completion_monitor: CompletionMonitor::PiHookFailure,
        })
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
    ) -> Result<()> {
        terminal::send_file(session, prompt_path)
    }

    fn terminal_initial_prompt(&self, directory: &Path, prompt: &str) -> Result<String> {
        let pending = read_pending_turn(directory)?
            .context("Pi initial turn correlation state is missing")?;
        Ok(correlated_prompt(prompt, &pending))
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
        let pending_path = directory.join(PENDING_TURN_FILE);
        let consuming_path = directory.join(PENDING_TURN_CONSUMING_FILE);
        super::super::rename_session_file(&pending_path, &consuming_path)
            .context("failed to claim the pending Pi turn result")?;
        let result = super::super::record_provider_result_for_claim(
            directory,
            FirstPartyCli::Pi,
            message,
            provider_session_id,
            turn_id,
            Some(&pending.claim_token),
        );
        if let Err(error) = result {
            let _ = super::super::rename_session_file(&consuming_path, &pending_path);
            return Err(error).context("failed to record the correlated Pi result");
        }
        let _ = super::super::remove_file_if_present(&consuming_path);
        Ok(())
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Pi does not expose Agent Bridge provider controls")
    }

    fn send_terminal_follow_up(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
    ) -> Result<()> {
        terminal::send_file(session, prompt_path)
    }

    fn prepare_terminal_follow_up(
        &self,
        directory: &Path,
        prompt: &str,
        claim_token: &str,
    ) -> Result<String> {
        let pending = install_pending_turn(directory, claim_token)?;
        Ok(correlated_prompt(prompt, &pending))
    }

    fn cancel_terminal_follow_up(&self, directory: &Path, claim_token: &str) -> Result<()> {
        cancel_pending_turn(directory, claim_token)
    }
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
    super::super::write_private(
        &directory.join(PENDING_TURN_FILE),
        &serde_json::to_vec_pretty(&pending)?,
    )?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingPiTurn>> {
    let Some(text) =
        super::super::read_regular_text_if_present(&directory.join(PENDING_TURN_FILE))?
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
    super::super::remove_file_if_present(&directory.join(PENDING_TURN_FILE))
}

fn correlated_prompt(prompt: &str, pending: &PendingPiTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Pi turn protocol]\nComplete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.\n{}",
        pending.marker
    )
}

fn correlated_response<'a>(message: &'a str, pending: &PendingPiTurn) -> Result<&'a str> {
    let body = message
        .trim_end()
        .strip_suffix(&pending.marker)
        .context("Pi response did not end with the expected turn marker")?
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
    let pending_path = directory.join(PENDING_TURN_FILE);
    let consuming_path = directory.join(PENDING_TURN_CONSUMING_FILE);
    super::super::rename_session_file(&pending_path, &consuming_path)
        .context("failed to claim the pending Pi turn failure")?;
    let result = super::super::record_provider_failure_for_claim(
        directory,
        FirstPartyCli::Pi,
        error,
        provider_session_id,
        turn_id,
        Some(&pending.claim_token),
    );
    if let Err(error) = result {
        let _ = super::super::rename_session_file(&consuming_path, &pending_path);
        return Err(error).context("failed to record the correlated Pi failure");
    }
    let _ = super::super::remove_file_if_present(&consuming_path);
    Ok(())
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
                    let _ = super::super::update_status(
                        &error_directory,
                        "failed",
                        None,
                        Some(format!("Pi result recovery monitor failed: {error:#}")),
                    );
                    let _ = super::super::release_turn_claim(&error_directory);
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
    let path = directory.join(HOOK_FAILURE_FILE);
    let Some(text) = super::super::read_regular_text_if_present(&path)? else {
        return Ok(false);
    };
    let signal: HookFailureSignal =
        serde_json::from_str(&text).context("invalid Pi hook failure recovery signal")?;
    let error = signal.error.trim();
    if error.is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    super::super::remove_file_if_present(&path)
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

	  pi.on("agent_start", (_event, ctx) => {
    if (undelivered && pending) {
      if (!persistHookFailure(pending, "a prior result remained undelivered")) {
        ctx.ui.notify("Agent Bridge still cannot recover the previous Pi result.", "warning");
        return;
      }
      undelivered = false;
	    }
	    pending = undefined;
	    activeClaimToken = readActiveClaimToken();
	  });

  pi.on("agent_end", (event, ctx) => {
    if (undelivered) return;
    pending = {
      ...lastAssistantOutcome(event.messages),
	      session_id: ctx.sessionManager.getSessionId(),
	      turn_id: ctx.sessionManager.getLeafId() ?? undefined,
	      agent_bridge_claim_token: activeClaimToken,
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
    use std::fs;

    use super::super::super::{
        SessionEvent, SessionStatus, TURN_CLAIM_FILE, acquire_turn_claim, event_paths, read_json,
        update_status, wait_for_event, write_json_atomic,
    };
    use super::*;

    fn claim_pending_turn(directory: &Path) -> PendingPiTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token.clone();
        claim.retain();
        install_pending_turn(directory, &token).unwrap()
    }

    fn marked(message: &str, pending: &PendingPiTurn) -> String {
        format!("{message}\n{}", pending.marker)
    }

    #[test]
    fn extension_reports_only_settled_results_without_changing_tool_policy() {
        let extension = bridge_extension();

        assert!(extension.contains("agent_start"));
        assert!(extension.contains("agent_end"));
        assert!(extension.contains("agent_settled"));
        assert!(extension.contains("stopReason"));
        assert!(extension.contains("agent_bridge_error"));
        assert!(extension.contains("agent_bridge_claim_token"));
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
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "session_id": "pi-session",
            "turn_id": "pi-turn",
            "last_assistant_message": marked("pi result", &pending),
            "agent_bridge_claim_token": pending.claim_token,
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "pi result");
        assert_eq!(event.provider_session_id.as_deref(), Some("pi-session"));
        assert_eq!(event.turn_id.as_deref(), Some("pi-turn"));
    }

    #[test]
    fn hook_transport_failure_signal_recovers_the_bridge_turn() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
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
        assert_eq!(status.state, "ready");
    }

    #[test]
    fn pi_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "session_id": "pi-session",
                    "turn_id": "pi-turn-1",
                    "last_assistant_message": marked("first result", &initial_pending),
                    "agent_bridge_claim_token": initial_pending.claim_token,
                }),
            )
            .unwrap();
        let _later_pending = claim_pending_turn(directory.path());
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "session_id": "pi-session",
                    "turn_id": "delayed-old-turn-with-a-new-id",
                    "last_assistant_message": "delayed old result",
                    "agent_bridge_claim_token": initial_pending.claim_token,
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }
}
