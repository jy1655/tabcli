use super::{
    CompletionMonitor, FollowUpTransport, InitialPromptTransport, LaunchContext, LaunchPlan,
    NativeProviderAdapter,
};
use anyhow::Result;
use std::{ffi::OsString, path::Path, time::Duration};

use super::super::terminal;

pub(super) static ADAPTER: ClaudeAdapter = ClaudeAdapter;

pub(super) struct ClaudeAdapter;

impl NativeProviderAdapter for ClaudeAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let settings_path = context.directory.join("claude-settings.json");
        super::super::write_json_atomic(
            &settings_path,
            &super::claude_hook_settings(context.bridge_executable),
        )?;
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("--settings"),
                settings_path.into_os_string(),
                OsString::from("--name"),
                OsString::from(context.title),
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

pub(super) fn hook_settings(executable: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": executable,
                    "args": ["native-hook", "claude"],
                    "timeout": 10
                }]
            }]
        }
    })
}
