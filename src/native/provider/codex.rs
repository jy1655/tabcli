use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter, ResumeContext, ResumePlan,
};
use agent_bridge::FirstPartyCli;
use anyhow::{Result, bail};
use std::{ffi::OsString, path::Path, time::Duration};

use super::super::terminal;

pub(super) static ADAPTER: CodexAdapter = CodexAdapter;

pub(super) struct CodexAdapter;

impl NativeProviderAdapter for CodexAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let notify = serde_json::to_string(&[
            context.bridge_executable.to_string_lossy().as_ref(),
            "native-hook",
            "codex",
        ])?;
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("-c"),
                OsString::from(format!("notify={notify}")),
                OsString::from("-C"),
                context.workspace.as_os_str().to_owned(),
            ],
            prompt_is_positional: true,
            completion_monitor: CompletionMonitor::Hook,
        })
    }

    fn prepare_resume(&self, _context: ResumeContext<'_>) -> Result<Option<ResumePlan>> {
        Ok(None)
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
        let message = codex_string(payload, "last-assistant-message")
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex notify payload has no assistant result"))?;
        let thread_id = codex_owned_string(payload, "thread-id");
        if let (Some(established), Some(incoming)) =
            (established_codex_thread(directory)?, thread_id.as_deref())
            && established != incoming
        {
            return Ok(());
        }
        super::super::record_provider_result(
            directory,
            FirstPartyCli::Codex,
            message,
            thread_id,
            codex_owned_string(payload, "turn-id"),
        )
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
}

fn codex_string<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn codex_owned_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    codex_string(payload, key).map(str::to_owned)
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

    #[test]
    fn codex_hook_owns_the_official_notify_payload_schema() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
        let payload = serde_json::json!({
            "thread-id": "codex-thread",
            "turn-id": "codex-turn",
            "last-assistant-message": "codex result",
        });

        ADAPTER.handle_hook(directory.path(), &payload).unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "codex result");
        assert_eq!(event.provider_session_id.as_deref(), Some("codex-thread"));
        assert_eq!(event.turn_id.as_deref(), Some("codex-turn"));
    }

    #[test]
    fn codex_hook_ignores_notify_events_from_a_different_thread() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
        let initial_claim = acquire_turn_claim(directory.path()).unwrap();
        initial_claim.retain();
        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn-1",
                    "last-assistant-message": "managed result",
                }),
            )
            .unwrap();
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();
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
        let claim = acquire_turn_claim(directory.path()).unwrap();
        claim.retain();

        ADAPTER
            .handle_hook(
                directory.path(),
                &serde_json::json!({
                    "thread-id": "managed-thread",
                    "turn-id": "managed-turn",
                    "last-assistant-message": "{\"title\":\"Requested title\"}",
                }),
            )
            .unwrap();

        let paths = event_paths(directory.path()).unwrap();
        let event: SessionEvent = read_json(&paths[0]).unwrap();
        assert_eq!(event.message, "{\"title\":\"Requested title\"}");
    }
}
