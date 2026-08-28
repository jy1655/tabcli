use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter,
};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, path::Path, time::Duration};

use super::super::terminal;

pub(super) static ADAPTER: CodexAdapter = CodexAdapter;

pub(super) struct CodexAdapter;

const PENDING_TURN_FILE: &str = "codex-pending-turn.json";
const PENDING_TURN_CONSUMING_FILE: &str = "codex-pending-turn.consuming.json";

#[derive(Debug, Deserialize, Serialize)]
struct PendingCodexTurn {
    schema: u32,
    claim_token: String,
    marker: String,
}

impl PendingCodexTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-codex-turn:{claim_token} -->"),
        })
    }
}

impl NativeProviderAdapter for CodexAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = super::super::current_turn_claim_token(context.directory)?
            .context("Codex launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let notify = serde_json::to_string(&[
            context.bridge_executable.to_string_lossy().as_ref(),
            "native-hook",
            "codex",
        ])?;
        let mut arguments = vec![
            OsString::from("-c"),
            OsString::from(format!("notify={notify}")),
            OsString::from("-C"),
            context.workspace.as_os_str().to_owned(),
        ];
        if !cfg!(windows) {
            arguments.push(OsString::from(correlated_prompt(context.prompt, &pending)));
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            completion_monitor: CompletionMonitor::Hook,
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
            .context("Codex initial turn correlation state is missing")?;
        Ok(correlated_prompt(prompt, &pending))
    }

    #[cfg(any(windows, test))]
    fn terminal_submit_count(&self) -> usize {
        2
    }

    fn follow_up_transport(&self) -> FollowUpTransport {
        // Replace this fallback when the supported Codex CLI exposes a verified
        // first-party cross-session input path for a live interactive session.
        FollowUpTransport::TerminalPasteFallback
    }

    fn new_cross_session_turn_id(&self) -> Result<String> {
        bail!("Codex does not support provider cross-session turns")
    }

    fn send_cross_session_message(
        &self,
        _context: CrossSessionMessageContext<'_>,
    ) -> CrossSessionMessageResult {
        Err(CrossSessionMessageFailure::not_sent(anyhow::anyhow!(
            "Codex does not support provider cross-session messages"
        )))
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        let raw_message = codex_string(payload, "last-assistant-message")
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex notify payload has no assistant result"))?;
        let Some(pending) = read_pending_turn(directory)? else {
            return Ok(());
        };
        let message = match correlated_response(raw_message, &pending) {
            Ok(message) => message,
            Err(_) if codex_input_correlates(payload, &pending) => raw_message,
            Err(_) => return Ok(()),
        };
        let thread_id = codex_owned_string(payload, "thread-id");
        if let (Some(established), Some(incoming)) =
            (established_codex_thread(directory)?, thread_id.as_deref())
            && established != incoming
        {
            return Ok(());
        }
        let pending_path = directory.join(PENDING_TURN_FILE);
        let consuming_path = directory.join(PENDING_TURN_CONSUMING_FILE);
        super::super::rename_session_file(&pending_path, &consuming_path)
            .context("failed to claim the pending Codex turn result")?;
        let result = super::super::record_provider_result_for_claim(
            directory,
            FirstPartyCli::Codex,
            message,
            thread_id,
            codex_owned_string(payload, "turn-id"),
            Some(&pending.claim_token),
        );
        if let Err(error) = result {
            let _ = super::super::rename_session_file(&consuming_path, &pending_path);
            return Err(error).context("failed to record the correlated Codex result");
        }
        let _ = super::super::remove_file_if_present(&consuming_path);
        Ok(())
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Codex does not expose Agent Bridge provider controls")
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
        bail!("invalid Codex turn claim token")
    }
    Ok(())
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingCodexTurn> {
    let pending = PendingCodexTurn::new(claim_token)?;
    super::super::write_private(
        &directory.join(PENDING_TURN_FILE),
        &serde_json::to_vec_pretty(&pending)?,
    )?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingCodexTurn>> {
    let Some(text) =
        super::super::read_regular_text_if_present(&directory.join(PENDING_TURN_FILE))?
    else {
        return Ok(None);
    };
    let pending: PendingCodexTurn =
        serde_json::from_str(&text).context("failed to parse the pending Codex turn")?;
    let expected = PendingCodexTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Codex turn correlation state")
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

fn correlated_prompt(prompt: &str, pending: &PendingCodexTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Codex turn metadata; do not include this metadata in the response]\n{}",
        pending.marker
    )
}

fn correlated_response<'a>(message: &'a str, pending: &PendingCodexTurn) -> Result<&'a str> {
    let body = message
        .trim_end()
        .strip_suffix(&pending.marker)
        .context("Codex response did not end with the expected turn marker")?
        .trim_end();
    if body.is_empty() {
        bail!("Codex correlated response contained no assistant text")
    }
    Ok(body)
}

fn codex_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn codex_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    codex_string(payload, key).map(str::to_owned)
}

fn codex_input_correlates(payload: &serde_json::Value, pending: &PendingCodexTurn) -> bool {
    if codex_string(payload, "type") != Some("agent-turn-complete") {
        return false;
    }
    payload
        .get("input-messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| messages.iter().rev().find_map(serde_json::Value::as_str))
        .is_some_and(|message| message.trim_end().ends_with(&pending.marker))
}

fn established_codex_thread(directory: &Path) -> Result<Option<String>> {
    for path in super::super::event_paths(directory)? {
        let event: super::super::SessionEvent = super::super::read_json(&path)?;
        if event.provider == FirstPartyCli::Codex.as_str()
            && let Some(thread_id) = event.provider_session_id
        {
            return Ok(Some(thread_id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::super::{
        SessionEvent, SessionStatus, TURN_CLAIM_FILE, acquire_turn_claim, event_paths, read_json,
        update_status,
    };
    use super::*;

    fn claim_pending_turn(directory: &Path) -> PendingCodexTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token.clone();
        claim.retain();
        install_pending_turn(directory, &token).unwrap()
    }

    fn marked(message: &str, pending: &PendingCodexTurn) -> String {
        format!("{message}\n{}", pending.marker)
    }

    #[test]
    fn codex_hook_owns_the_official_notify_payload_schema() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "thread-id": "codex-thread",
            "turn-id": "codex-turn",
            "last-assistant-message": marked("codex result", &pending),
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "codex result");
        assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
    }

    #[test]
    fn codex_hook_correlates_exact_output_from_the_official_input_messages() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());
        let payload = serde_json::json!({
            "type": "agent-turn-complete",
            "thread-id": "codex-thread",
            "turn-id": "codex-turn",
            "input-messages": [format!(
                "Reply with exactly EXACT_OUTPUT and nothing else.\n{}",
                pending.marker
            )],
            "last-assistant-message": "EXACT_OUTPUT",
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "EXACT_OUTPUT");
        assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
    }

    #[test]
    fn codex_hook_ignores_notify_events_from_a_different_thread() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": marked("managed result", &initial_pending),
                }),
            )
            .unwrap();
        let _pending = claim_pending_turn(directory.path());
        update_status(directory.path(), "claimed", None, None).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "title-thread",
                    "turn-id": "title-turn",
                    "last-assistant-message": "{\"title\":\"Generated title\"}",
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
        let status: SessionStatus = read_json(&directory.path().join("status.json")).unwrap();
        assert_eq!(status.state, "working");
    }

    #[test]
    fn codex_hook_preserves_a_legitimate_title_shaped_result() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let pending = claim_pending_turn(directory.path());

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn",
                    "last-assistant-message": marked("{\"title\":\"Requested title\"}", &pending),
                }),
            )
            .unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "{\"title\":\"Requested title\"}");
    }

    #[test]
    fn codex_hook_does_not_bind_the_first_foreign_notify_event() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let _pending = claim_pending_turn(directory.path());

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "foreign-title-thread",
                    "turn-id": "foreign-title-turn",
                    "last-assistant-message": "{\"title\":\"Generated title\"}",
                }),
            )
            .unwrap();

        assert!(event_paths(directory.path()).unwrap().is_empty());
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }

    #[test]
    fn codex_hook_does_not_bind_a_delayed_new_turn_to_a_later_claim() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_pending = claim_pending_turn(directory.path());
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": marked("first result", &initial_pending),
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
                    "type": "agent-turn-complete",
                    "thread-id": "managed-thread",
                    "turn-id": "delayed-old-turn-with-a-new-id",
                    "input-messages": [format!("old prompt\n{}", initial_pending.marker)],
                    "last-assistant-message": "delayed old result",
                }),
            )
            .unwrap();

        assert_eq!(event_paths(directory.path()).unwrap().len(), 1);
        assert!(directory.path().join(TURN_CLAIM_FILE).exists());
    }
}
