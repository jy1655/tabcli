use super::{
    CompletionMonitor, FollowUpTransport, InitialPromptTransport, LaunchContext, LaunchPlan,
    NativeProviderAdapter, ResumeContext, ResumePlan,
};
use anyhow::Result;
use std::{ffi::OsString, path::Path, time::Duration};

use super::super::terminal;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;

pub(super) struct AgyAdapter;

impl NativeProviderAdapter for AgyAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let log_path = context.directory.join("agy.log");
        let mut arguments = vec![
            OsString::from("--log-file"),
            log_path.as_os_str().to_owned(),
        ];
        if !cfg!(windows) {
            arguments.extend([
                OsString::from("--prompt-interactive"),
                OsString::from(context.prompt),
            ]);
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            completion_monitor: CompletionMonitor::AgyTranscript { log_path },
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
        // Agy redraws its composer while refreshing authentication, models, and
        // extensions after reporting that the CLI is ready. Input delivered in
        // that interval is discarded by the native Windows TUI.
        Duration::from_secs(12)
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
