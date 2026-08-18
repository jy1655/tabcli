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
        super::super::record_provider_result(
            directory,
            FirstPartyCli::Codex,
            message,
            codex_owned_string(payload, "thread-id"),
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

#[cfg(test)]
mod tests {
    use super::super::super::{SessionEvent, event_paths, read_json, update_status};
    use super::*;

    #[test]
    fn codex_hook_owns_the_official_notify_payload_schema() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("events")).unwrap();
        update_status(directory.path(), "working", None, None).unwrap();
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
}
