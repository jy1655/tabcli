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
    // Caller environment variables the adapter refuses to pass to its provider process.
    // Each adapter owns this list; the shared launcher only applies it.
    pub(super) environment_removals: &'static [&'static str],
}

// Launch inputs for a session that continues a closed session's provider conversation.
// The provider conversation identity comes from the closed session's recorded event; the
// directory is the new session's own private state, never the source's.
pub(super) struct ResumeContext<'a> {
    pub(super) bridge_executable: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) provider_session_id: &'a str,
}

// The provider arguments that reopen a conversation. A resume plan never carries an initial
// prompt: the reopened session receives it through the provider's initial-prompt transport,
// exactly as a fresh `ask` does, so the same delivery evidence applies.
#[derive(Debug)]
pub(super) struct ResumePlan {
    pub(super) arguments: Vec<OsString>,
    pub(super) completion_monitor: CompletionMonitor,
    pub(super) environment_removals: &'static [&'static str],
}

// A reopened session whose provider process is running. `directory` is the new session's
// own state. With `wait_for_registration` the adapter waits until `deadline` for the
// provider's own registration of that process (the post-launch check, before the initial
// prompt exists in the process); without it the adapter answers from the registry as it is
// now and treats a missing registration as a failure (the re-scan immediately before the
// initial prompt and before every follow-up delivery).
#[derive(Clone, Copy)]
pub(super) struct ResumedSessionContext<'a> {
    pub(super) directory: &'a Path,
    pub(super) provider_session_id: &'a str,
    pub(super) deadline: Instant,
    pub(super) wait_for_registration: bool,
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
    terminal_fallback_allowed: bool,
}

impl CrossSessionMessageFailure {
    pub(super) fn not_sent(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
            retryable_discovery_failure: false,
            terminal_fallback_allowed: false,
        }
    }

    pub(super) fn retryable_discovery_failure(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
            retryable_discovery_failure: true,
            terminal_fallback_allowed: false,
        }
    }

    pub(super) fn delivery_uncertain(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: true,
            retryable_discovery_failure: false,
            terminal_fallback_allowed: false,
        }
    }

    pub(super) fn terminal_fallback(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
            retryable_discovery_failure: false,
            terminal_fallback_allowed: true,
        }
    }

    pub(super) fn delivery_may_have_occurred(&self) -> bool {
        self.delivery_may_have_occurred
    }

    pub(super) fn should_retry_discovery(&self) -> bool {
        self.retryable_discovery_failure
    }

    pub(super) fn allows_terminal_fallback(&self) -> bool {
        self.terminal_fallback_allowed
    }

    pub(super) fn into_error(self) -> anyhow::Error {
        self.error
    }
}

pub(super) type CrossSessionMessageResult = std::result::Result<(), CrossSessionMessageFailure>;

#[derive(Debug)]
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
    ProviderCrossSessionMessageWithTerminalPasteFallback,
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
            Self::ProviderCrossSessionMessageWithTerminalPasteFallback => {
                "provider-cross-session-message-with-terminal-paste-fallback"
            }
        }
    }
}

trait NativeProviderAdapter: Sync {
    fn workspace_trust(
        &self,
        workspace: &Path,
        homes: &super::consent::Homes,
    ) -> Result<super::consent::Trust>;
    fn workspace_trust_key(&self, screen: &str, workspace: &Path) -> Option<terminal::DialogKey>;
    fn diagnose(&self, context: super::doctor::Context<'_>) -> Vec<super::doctor::Check>;
    // Caller-environment variables removed from every short-lived provider query the
    // bridge runs (version preflight, doctor probes). Each adapter states its own list;
    // there is no shared default.
    fn probe_environment_removals(&self) -> &'static [&'static str];
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
    // Read-only reopen gate. Each adapter states whether the recorded provider conversation
    // can be continued in a new process on this platform and whether the provider's own
    // ownership evidence shows the conversation is still held by a live process. There is no
    // shared default: an adapter that cannot prove ownership must refuse with its reason.
    fn verify_reopen_available(&self, provider_session_id: &str) -> Result<()>;
    fn prepare_resume(&self, context: ResumeContext<'_>) -> Result<ResumePlan>;
    // Ownership check for a reopened session, run after launch and again at every delivery
    // boundary. Once the provider's own evidence shows the new process registered as the
    // conversation's holder, the adapter reports every OTHER live process that holds the
    // same conversation. A non-empty answer is a conflict the shared layer refuses the
    // delivery for; an error is a verification failure the shared layer also refuses for,
    // because an unverifiable conversation is treated as shared. This is best-effort
    // detection at each call, not exclusion. An adapter that refuses reopen refuses here as
    // well, because this call is only reachable after its resume plan ran.
    fn other_resumed_conversation_holders(
        &self,
        context: ResumedSessionContext<'_>,
    ) -> Result<Vec<u32>>;
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

pub(super) fn workspace_trust(
    provider: FirstPartyCli,
    workspace: &Path,
    homes: &super::consent::Homes,
) -> Result<super::consent::Trust> {
    adapter(provider).workspace_trust(workspace, homes)
}

pub(super) fn workspace_trust_key(
    provider: FirstPartyCli,
    screen: &str,
    workspace: &Path,
) -> Option<terminal::DialogKey> {
    adapter(provider).workspace_trust_key(screen, workspace)
}

pub(super) fn diagnose(
    provider: FirstPartyCli,
    context: super::doctor::Context<'_>,
) -> Vec<super::doctor::Check> {
    adapter(provider).diagnose(context)
}

pub(super) fn result_timeout_diagnostic(
    provider: FirstPartyCli,
) -> Option<super::doctor::ResultTimeoutDiagnostic> {
    match provider {
        FirstPartyCli::Agy => Some(agy::result_timeout_diagnostic()),
        FirstPartyCli::Pi => Some(pi::result_timeout_diagnostic()),
        FirstPartyCli::Codex | FirstPartyCli::Claude => None,
    }
}

pub(super) fn prepare_launch(
    provider: FirstPartyCli,
    context: LaunchContext<'_>,
) -> Result<LaunchPlan> {
    adapter(provider).prepare_launch(context)
}

pub(super) fn verify_reopen_available(
    provider: FirstPartyCli,
    provider_session_id: &str,
) -> Result<()> {
    adapter(provider).verify_reopen_available(provider_session_id)
}

pub(super) fn probe_environment_removals(provider: FirstPartyCli) -> &'static [&'static str] {
    adapter(provider).probe_environment_removals()
}

pub(super) fn prepare_resume(
    provider: FirstPartyCli,
    context: ResumeContext<'_>,
) -> Result<ResumePlan> {
    adapter(provider).prepare_resume(context)
}

pub(super) fn other_resumed_conversation_holders(
    provider: FirstPartyCli,
    context: ResumedSessionContext<'_>,
) -> Result<Vec<u32>> {
    adapter(provider).other_resumed_conversation_holders(context)
}

// Points the Claude adapter's session-registry reads at a fixture for the calling thread,
// so a launch-boundary test can mutate the registry between the reopen gates without
// touching the real `~/.claude/sessions`.
#[cfg(test)]
pub(super) fn override_claude_session_registry_for_test(registry: Option<PathBuf>) {
    claude::override_session_registry_for_test(registry);
}

// Applies an adapter's caller-environment removals to a provider process before it
// starts. Removal is recorded on the command itself so a test can prove the variable
// never reaches the child without spawning it.
pub(super) fn apply_environment_removals(command: &mut std::process::Command, removals: &[&str]) {
    for variable in removals {
        command.env_remove(variable);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_removals_are_recorded_on_the_command_before_spawn() {
        let mut command = std::process::Command::new("provider");
        command.env("CLAUDE_CODE_CHILD_SESSION", "1");
        apply_environment_removals(&mut command, &["CLAUDE_CODE_CHILD_SESSION", "CLAUDECODE"]);
        let removed = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            removed,
            vec![
                std::ffi::OsString::from("CLAUDECODE"),
                std::ffi::OsString::from("CLAUDE_CODE_CHILD_SESSION"),
            ]
        );
    }
}
