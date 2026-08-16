use super::{
    CompletionMonitor, FollowUpTransport, LaunchContext, LaunchPlan, NativeProviderAdapter,
};
use anyhow::Result;
use std::{ffi::OsString, path::Path};

use super::super::terminal;

pub(super) static ADAPTER: PiAdapter = PiAdapter;

pub(super) struct PiAdapter;

impl NativeProviderAdapter for PiAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let extension_path = context.directory.join("pi-agent-bridge.js");
        super::super::write_private(
            &extension_path,
            super::super::pi_bridge_extension().as_bytes(),
        )?;
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("--extension"),
                extension_path.into_os_string(),
                OsString::from("--name"),
                OsString::from(context.title),
            ],
            prompt_is_positional: true,
            completion_monitor: CompletionMonitor::PiHookFailure,
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
