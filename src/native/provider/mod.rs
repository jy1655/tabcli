mod agy;
mod claude;
mod codex;
mod pi;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{ffi::OsString, path::Path, path::PathBuf, time::Instant};

use super::{delivery::Transport, terminal};

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

// A reopened session whose provider process is running; directory is its own state.
#[derive(Clone, Copy)]
pub(super) struct ResumedSessionContext<'a> {
    pub(super) directory: &'a Path,
    pub(super) provider_session_id: &'a str,
}

#[derive(Clone, Copy)]
#[cfg_attr(windows, allow(dead_code))]
pub(super) struct CrossSessionMessageContext<'a> {
    pub(super) bridge_executable: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) provider_path: &'a Path,
    // Adapter-selected turn correlation value, not a Bridge Request id:
    // Codex uses the Claim token; Claude uses its adapter-generated claude-turn id.
    pub(super) correlation_id: &'a str,
    pub(super) prompt: &'a str,
    pub(super) deadline: Instant,
}

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

pub(super) enum CancelSupport {
    Supported,
    Unsupported(String),
}

pub(super) fn cancel_support(provider: FirstPartyCli, directory: &Path) -> Result<CancelSupport> {
    adapter(provider).cancel_support(directory)
}

pub(super) trait ReopenSupport: Sync {
    fn verify_available(&self, provider_session_id: &str) -> Result<()>;
    fn prepare(&self, context: ResumeContext<'_>) -> Result<ResumePlan>;
    // Wait for this session's registration until the supplied deadline, then report other
    // live holders. Registry errors are not waited out.
    fn other_holders_after_registration(
        &self,
        context: ResumedSessionContext<'_>,
        deadline: Instant,
    ) -> Result<Vec<u32>>;
    // Succeeds only when this session's own registration is verified now.
    fn other_holders_now(&self, context: ResumedSessionContext<'_>) -> Result<Vec<u32>>;
}

trait NativeProviderAdapter: Sync {
    fn initial_transport(&self) -> Box<dyn Transport>;
    fn follow_up_transport(&self) -> Box<dyn Transport>;
    fn cancel_support(&self, directory: &Path) -> Result<CancelSupport>;
    fn launch_trust(&self) -> super::consent::LaunchTrust;
    fn workspace_trust(
        &self,
        workspace: &Path,
        homes: &super::consent::Homes,
    ) -> Result<super::consent::Trust>;
    fn workspace_trust_key(&self, screen: &str, workspace: &Path) -> Option<terminal::DialogKey>;
    fn result_timeout_diagnostic(&self) -> Option<super::doctor::ResultTimeoutDiagnostic>;
    fn diagnose(&self, context: super::doctor::Context<'_>) -> Vec<super::doctor::Check>;
    // Caller-environment variables removed from every short-lived provider query the
    // bridge runs (version preflight, doctor probes). Each adapter states its own list;
    // there is no shared default.
    fn probe_environment_removals(&self) -> &'static [&'static str];
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
    // Ok means an implementation exists, not availability for this platform or conversation.
    // Each operation verifies its own availability.
    fn reopen(&self) -> Result<&'static dyn ReopenSupport>;
    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()>;
    fn run_control(&self, arguments: &[String]) -> Result<()>;
}

fn adapter(provider: FirstPartyCli) -> &'static dyn NativeProviderAdapter {
    match provider {
        FirstPartyCli::Codex => &codex::ADAPTER,
        FirstPartyCli::Claude => &claude::ADAPTER,
        FirstPartyCli::Agy => &agy::ADAPTER,
        FirstPartyCli::Pi => &pi::ADAPTER,
    }
}

pub(super) fn launch_trust(provider: FirstPartyCli) -> super::consent::LaunchTrust {
    adapter(provider).launch_trust()
}

// Native trust waits accept any adapter-declared application marker, independently
// of the target provider; authorization remains a separate required observation.
pub(super) fn launch_trust_markers() -> impl Iterator<Item = &'static str> {
    [
        FirstPartyCli::Codex,
        FirstPartyCli::Claude,
        FirstPartyCli::Agy,
        FirstPartyCli::Pi,
    ]
    .into_iter()
    .filter_map(|provider| launch_trust(provider).applied_marker)
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
    adapter(provider).result_timeout_diagnostic()
}

pub(super) fn prepare_launch(
    provider: FirstPartyCli,
    context: LaunchContext<'_>,
) -> Result<LaunchPlan> {
    adapter(provider).prepare_launch(context)
}

pub(super) fn reopen(provider: FirstPartyCli) -> Result<&'static dyn ReopenSupport> {
    adapter(provider).reopen()
}

pub(super) fn probe_environment_removals(provider: FirstPartyCli) -> &'static [&'static str] {
    adapter(provider).probe_environment_removals()
}

// Points the Claude adapter's session-registry reads at a fixture for the calling thread,
// so a launch-boundary test can mutate the registry between the reopen gates without
// touching the real `~/.claude/sessions`.
#[cfg(test)]
pub(super) fn override_claude_session_registry_for_test(registry: Option<PathBuf>) {
    claude::override_session_registry_for_test(registry);
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

pub(super) fn initial_transport(provider: FirstPartyCli) -> Box<dyn Transport> {
    adapter(provider).initial_transport()
}
pub(super) fn follow_up_transport(provider: FirstPartyCli) -> Box<dyn Transport> {
    adapter(provider).follow_up_transport()
}

#[cfg(test)]
fn transport_test_context<'a>(
    directory: &'a Path,
    surface: &'a terminal::TerminalSession,
    kind: super::delivery::TurnKind,
) -> super::delivery::TurnContext<'a> {
    super::delivery::TurnContext {
        kind,
        directory,
        session_id: "session-transport",
        claim_token: "1-2-3",
        surface,
        provider_path: Path::new("provider"),
        bridge_executable: Path::new("bridge"),
        prompt: "provider transport framing",
        deadline: Instant::now() + std::time::Duration::from_secs(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_declare_launch_trust_facts() {
        for (provider, answers, bypass, marker) in [
            (
                FirstPartyCli::Codex,
                false,
                false,
                Some("codex-config-override"),
            ),
            (FirstPartyCli::Claude, true, true, None),
            (FirstPartyCli::Agy, true, false, None),
            (FirstPartyCli::Pi, false, false, Some("pi-approve-once")),
        ] {
            let facts = launch_trust(provider);
            assert_eq!(facts.answers_launch_dialog, answers, "{provider:?}");
            assert_eq!(facts.bypass_suppresses_dialog, bypass, "{provider:?}");
            assert_eq!(facts.applied_marker, marker, "{provider:?}");
        }
    }
}
