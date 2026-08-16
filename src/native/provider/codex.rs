use super::{
    CompletionMonitor, FollowUpTransport, LaunchContext, LaunchPlan, NativeProviderAdapter,
};
use anyhow::Result;
use std::{ffi::OsString, path::Path};

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
