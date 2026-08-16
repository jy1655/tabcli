mod agy;
mod claude;
mod codex;
mod pi;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{ffi::OsString, path::Path, path::PathBuf};

use super::terminal;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FollowUpTransport {
    TerminalPasteFallback,
}

impl FollowUpTransport {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalPasteFallback => "terminal-paste-fallback",
        }
    }
}

trait NativeProviderAdapter: Sync {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
    fn follow_up_transport(&self) -> FollowUpTransport;
    fn send_follow_up(&self, session: &terminal::TerminalSession, prompt_path: &Path)
    -> Result<()>;
}

fn adapter(provider: FirstPartyCli) -> &'static dyn NativeProviderAdapter {
    match provider {
        FirstPartyCli::Codex => &codex::ADAPTER,
        FirstPartyCli::Claude => &claude::ADAPTER,
        FirstPartyCli::Agy => &agy::ADAPTER,
        FirstPartyCli::Pi => &pi::ADAPTER,
    }
}

pub(super) fn prepare_launch(
    provider: FirstPartyCli,
    context: LaunchContext<'_>,
) -> Result<LaunchPlan> {
    adapter(provider).prepare_launch(context)
}

pub(super) fn follow_up_transport(provider: FirstPartyCli) -> FollowUpTransport {
    adapter(provider).follow_up_transport()
}

pub(super) fn send_follow_up(
    provider: FirstPartyCli,
    session: &terminal::TerminalSession,
    prompt_path: &Path,
) -> Result<()> {
    adapter(provider).send_follow_up(session, prompt_path)
}

pub(super) fn claude_hook_settings(executable: &Path) -> serde_json::Value {
    claude::hook_settings(executable)
}
