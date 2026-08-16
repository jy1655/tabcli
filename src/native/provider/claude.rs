use super::{
    CompletionMonitor, FollowUpTransport, InitialPromptTransport, LaunchContext, LaunchPlan,
    NativeProviderAdapter, ResumeContext, ResumePlan,
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
        let mut arguments = vec![
            OsString::from("--settings"),
            settings_path.into_os_string(),
            OsString::from("--name"),
            OsString::from(context.title),
        ];
        if cfg!(windows) {
            arguments.push(OsString::from("--print"));
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: !cfg!(windows),
            completion_monitor: CompletionMonitor::Hook,
        })
    }

    fn prepare_resume(&self, context: ResumeContext<'_>) -> Result<Option<ResumePlan>> {
        if !cfg!(windows) {
            return Ok(None);
        }
        let settings_path = context.directory.join("claude-settings.json");
        super::super::write_json_atomic(
            &settings_path,
            &super::claude_hook_settings(context.bridge_executable),
        )?;
        Ok(Some(ResumePlan {
            arguments: vec![
                OsString::from("--settings"),
                settings_path.into_os_string(),
                OsString::from("--print"),
                OsString::from("--resume"),
                OsString::from(context.provider_session_id),
            ],
        }))
    }

    fn initial_prompt_transport(&self) -> InitialPromptTransport {
        if cfg!(windows) {
            InitialPromptTransport::ProviderStdin
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
        if cfg!(windows) {
            FollowUpTransport::ProviderResumeSupervisor
        } else {
            FollowUpTransport::TerminalPasteFallback
        }
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
