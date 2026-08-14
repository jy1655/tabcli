mod agy;
mod claude;
mod codex;
mod pi;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{ffi::OsString, path::Path, path::PathBuf};

pub(super) struct LaunchContext<'a> {
    pub(super) bridge_executable: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) title: &'a str,
    pub(super) prompt: &'a str,
}

pub(super) struct LaunchPlan {
    pub(super) arguments: Vec<OsString>,
    pub(super) prompt_is_positional: bool,
    pub(super) completion_monitor: CompletionMonitor,
}

pub(super) enum CompletionMonitor {
    Hook,
    AgyTranscript { log_path: PathBuf },
    PiHookFailure,
}

trait NativeProviderAdapter: Sync {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
}

pub(super) fn prepare_launch(
    provider: FirstPartyCli,
    context: LaunchContext<'_>,
) -> Result<LaunchPlan> {
    let adapter: &dyn NativeProviderAdapter = match provider {
        FirstPartyCli::Codex => &codex::ADAPTER,
        FirstPartyCli::Claude => &claude::ADAPTER,
        FirstPartyCli::Agy => &agy::ADAPTER,
        FirstPartyCli::Pi => &pi::ADAPTER,
    };
    adapter.prepare_launch(context)
}

pub(super) fn claude_hook_settings(executable: &Path) -> serde_json::Value {
    claude::hook_settings(executable)
}
