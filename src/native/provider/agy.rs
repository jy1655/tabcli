use super::{
    CompletionMonitor, FollowUpTransport, LaunchContext, LaunchPlan, NativeProviderAdapter,
};
use anyhow::Result;
use std::{ffi::OsString, path::Path};

use super::super::terminal;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;

pub(super) struct AgyAdapter;

impl NativeProviderAdapter for AgyAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let log_path = context.directory.join("agy.log");
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("--log-file"),
                log_path.as_os_str().to_owned(),
                OsString::from("--prompt-interactive"),
                OsString::from(context.prompt),
            ],
            prompt_is_positional: false,
            completion_monitor: CompletionMonitor::AgyTranscript { log_path },
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
