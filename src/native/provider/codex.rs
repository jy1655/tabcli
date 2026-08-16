use super::{
    CompletionMonitor, FollowUpTransport, InitialPromptTransport, LaunchContext, LaunchPlan,
    NativeProviderAdapter,
};
use anyhow::Result;
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

    fn follow_up_transport(&self) -> FollowUpTransport {
        FollowUpTransport::TerminalPasteFallback
    }

    fn send_follow_up(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
    ) -> Result<()> {
        terminal::send_file(session, prompt_path)
    }
}
