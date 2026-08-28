mod agy;
mod claude;
mod codex;
mod pi;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{
    ffi::OsString,
    path::Path,
    path::PathBuf,
    time::{Duration, Instant},
};

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

#[derive(Clone, Copy)]
#[cfg_attr(windows, allow(dead_code))]
pub(super) struct CrossSessionMessageContext<'a> {
    pub(super) bridge_executable: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) provider_path: &'a Path,
    pub(super) request_id: &'a str,
    pub(super) prompt: &'a str,
    pub(super) deadline: Instant,
}

#[derive(Debug)]
pub(super) struct CrossSessionMessageFailure {
    error: anyhow::Error,
    delivery_may_have_occurred: bool,
    retryable_discovery_failure: bool,
}

impl CrossSessionMessageFailure {
    pub(super) fn not_sent(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
            retryable_discovery_failure: false,
        }
    }

    pub(super) fn retryable_discovery_failure(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
            retryable_discovery_failure: true,
        }
    }

    pub(super) fn delivery_uncertain(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: true,
            retryable_discovery_failure: false,
        }
    }

    pub(super) fn delivery_may_have_occurred(&self) -> bool {
        self.delivery_may_have_occurred
    }

    pub(super) fn should_retry_discovery(&self) -> bool {
        self.retryable_discovery_failure
    }

    pub(super) fn into_error(self) -> anyhow::Error {
        self.error
    }
}

pub(super) type CrossSessionMessageResult = std::result::Result<(), CrossSessionMessageFailure>;

pub(super) enum CompletionMonitor {
    Hook,
    AgyTranscript { log_path: PathBuf },
    PiHookFailure,
}

pub(super) struct ActiveCompletionMonitor(ActiveCompletionMonitorInner);

enum ActiveCompletionMonitorInner {
    None,
    Agy(agy::AgyMonitor),
    Pi(pi::PiFailureMonitor),
}

impl CompletionMonitor {
    pub(super) fn start(self, directory: &Path) -> Result<ActiveCompletionMonitor> {
        match self {
            Self::Hook => Ok(ActiveCompletionMonitor(ActiveCompletionMonitorInner::None)),
            Self::AgyTranscript { log_path } => Ok(ActiveCompletionMonitor(
                ActiveCompletionMonitorInner::Agy(agy::AgyMonitor::start(directory, &log_path)?),
            )),
            Self::PiHookFailure => Ok(ActiveCompletionMonitor(ActiveCompletionMonitorInner::Pi(
                pi::PiFailureMonitor::start(directory)?,
            ))),
        }
    }
}

impl ActiveCompletionMonitor {
    pub(super) fn stop(self) -> Result<()> {
        match self.0 {
            ActiveCompletionMonitorInner::None => Ok(()),
            ActiveCompletionMonitorInner::Agy(monitor) => monitor.stop(),
            ActiveCompletionMonitorInner::Pi(monitor) => monitor.stop(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FollowUpTransport {
    TerminalPasteFallback,
    ProviderCrossSessionMessage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitialPromptTransport {
    ProviderArgument,
    ProviderCrossSessionMessageAfterLaunch,
    TerminalPasteAfterLaunch,
}

impl FollowUpTransport {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::TerminalPasteFallback => "terminal-paste-fallback",
            Self::ProviderCrossSessionMessage => "provider-cross-session-message",
        }
    }
}

trait NativeProviderAdapter: Sync {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
    fn initial_prompt_transport(&self) -> InitialPromptTransport;
    fn initial_prompt_ready_delay(&self) -> Duration;
    fn send_initial_prompt(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult;
    fn terminal_initial_prompt(&self, directory: &Path, prompt: &str) -> Result<String>;
    #[cfg(any(windows, test))]
    fn terminal_submit_count(&self) -> usize;
    fn follow_up_transport(&self) -> FollowUpTransport;
    fn new_cross_session_turn_id(&self) -> Result<String>;
    fn send_cross_session_message(
        &self,
        context: CrossSessionMessageContext<'_>,
    ) -> CrossSessionMessageResult;
    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()>;
    fn run_control(&self, arguments: &[String]) -> Result<()>;
    fn send_terminal_follow_up(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult;
    fn prepare_terminal_follow_up(
        &self,
        directory: &Path,
        prompt: &str,
        claim_token: &str,
    ) -> Result<String>;
    fn cancel_terminal_follow_up(&self, directory: &Path, claim_token: &str) -> Result<()>;
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

pub(super) fn initial_prompt_transport(provider: FirstPartyCli) -> InitialPromptTransport {
    adapter(provider).initial_prompt_transport()
}

pub(super) fn initial_prompt_ready_delay(provider: FirstPartyCli) -> Duration {
    adapter(provider).initial_prompt_ready_delay()
}

#[cfg(any(windows, test))]
pub(super) fn terminal_submit_count(provider: FirstPartyCli) -> usize {
    adapter(provider).terminal_submit_count()
}

pub(super) fn validate_terminal_send_budget(
    provider: FirstPartyCli,
    terminal_kind: terminal::TerminalKind,
    timeout: Duration,
) -> Result<()> {
    #[cfg(windows)]
    {
        validate_terminal_send_budget_for_platform(provider, terminal_kind, timeout, true)
    }
    #[cfg(not(windows))]
    {
        let _ = (provider, terminal_kind, timeout);
        Ok(())
    }
}

#[cfg(any(windows, test))]
pub(super) fn validate_terminal_send_budget_for_platform(
    provider: FirstPartyCli,
    terminal_kind: terminal::TerminalKind,
    timeout: Duration,
    windows: bool,
) -> Result<()> {
    if windows
        && terminal_kind == terminal::TerminalKind::WindowsConsole
        && !terminal::windows_console_submit_delays_fit(terminal_submit_count(provider), timeout)
    {
        anyhow::bail!(
            "Windows console submission delays do not fit inside the remaining turn timeout"
        )
    }
    Ok(())
}

pub(super) fn send_terminal_follow_up(
    provider: FirstPartyCli,
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> terminal::TerminalSendResult {
    adapter(provider).send_terminal_follow_up(session, prompt_path, deadline)
}

pub(super) fn send_cross_session_message(
    provider: FirstPartyCli,
    context: CrossSessionMessageContext<'_>,
) -> CrossSessionMessageResult {
    adapter(provider).send_cross_session_message(context)
}

pub(super) fn new_cross_session_turn_id(provider: FirstPartyCli) -> Result<String> {
    adapter(provider).new_cross_session_turn_id()
}

pub(super) fn handle_hook(
    provider: FirstPartyCli,
    directory: &Path,
    payload: &serde_json::Value,
) -> Result<()> {
    adapter(provider).handle_hook(directory, payload)
}

pub(super) fn run_control(provider: FirstPartyCli, arguments: &[String]) -> Result<()> {
    adapter(provider).run_control(arguments)
}

pub(super) fn send_initial_prompt(
    provider: FirstPartyCli,
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> terminal::TerminalSendResult {
    adapter(provider).send_initial_prompt(session, prompt_path, deadline)
}

pub(super) fn terminal_initial_prompt(
    provider: FirstPartyCli,
    directory: &Path,
    prompt: &str,
) -> Result<String> {
    adapter(provider).terminal_initial_prompt(directory, prompt)
}

pub(super) fn prepare_terminal_follow_up(
    provider: FirstPartyCli,
    directory: &Path,
    prompt: &str,
    claim_token: &str,
) -> Result<String> {
    adapter(provider).prepare_terminal_follow_up(directory, prompt, claim_token)
}

pub(super) fn cancel_terminal_follow_up(
    provider: FirstPartyCli,
    directory: &Path,
    claim_token: &str,
) -> Result<()> {
    adapter(provider).cancel_terminal_follow_up(directory, claim_token)
}
