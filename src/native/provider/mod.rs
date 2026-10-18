mod agy;
mod capped_output;
// Named from `crate::native` so that the reopen tests can point the Claude session-registry
// reads at a fixture through the Claude module itself (`override_session_registry_for_test`).
pub(super) mod claude;
mod codex;
mod pi;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{ffi::OsString, fmt::Debug, path::Path, path::PathBuf, time::Instant};

use super::{delivery::Transport, terminal};

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub(super) struct EmittedQuestion {
    pub(super) title: String,
    pub(super) options: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub(super) struct QuestionEmission {
    pub(super) id: String,
    pub(super) questions: Vec<EmittedQuestion>,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum QuestionObservation {
    Complete,
    Incomplete,
    Unreadable,
}

pub(super) struct QuestionEmissions {
    pub(super) items: Vec<QuestionEmission>,
    pub(super) observation: QuestionObservation,
}

/// Reads under the Snapshot's lifecycle lock; a snapshot without it gets no question fields.
pub(super) fn question_emissions(
    reader: &super::session::Reader,
    provider: &str,
    locked: bool,
    receipt: &super::session::requests::Receipt,
) -> Option<QuestionEmissions> {
    if provider == "codex" && locked {
        codex::questions::read(reader, receipt)
    } else {
        None
    }
}

// The manifest's policy inputs travel with every launch context. Each adapter maps them to
// its CLI's flags itself through the public mappings; no shared code assembles them.
pub(super) struct LaunchContext<'a> {
    pub(super) bridge_executable: &'a Path,
    pub(super) directory: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) title: &'a str,
    pub(super) prompt: &'a str,
    pub(super) yolo: bool,
    pub(super) model: Option<&'a str>,
    pub(super) effort: Option<&'a str>,
}

// How the initial prompt reaches the provider process, declared by the adapter from its own
// initial transport. The wrapper acts on it right after the process record: an accepted
// prompt is removed and the session runs; an awaited one stays until delivery sends it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitialPromptDisposition {
    AcceptedAtSpawn,
    AwaitingDelivery,
}

impl InitialPromptDisposition {
    pub(super) fn of(transport: &dyn Transport) -> Self {
        match transport.facts().readiness {
            super::delivery::Readiness::LaunchArgument => Self::AcceptedAtSpawn,
            super::delivery::Readiness::AfterLaunch { .. } => Self::AwaitingDelivery,
        }
    }
}

// The complete provider command of a fresh session: every argument in its final order,
// including the adapter's mapping of the policy inputs and the prompt's place.
pub(super) struct LaunchPlan {
    pub(super) arguments: Vec<OsString>,
    pub(super) initial_prompt: InitialPromptDisposition,
    // The adapter's completion watcher, unstarted: the wrapper starts it right before the
    // provider command exists and stops it after the provider process ends.
    pub(super) watcher: Box<dyn CompletionWatcher>,
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
    pub(super) yolo: bool,
    pub(super) model: Option<&'a str>,
    pub(super) effort: Option<&'a str>,
}

// The provider arguments that reopen a conversation. A resume plan never carries an initial
// prompt: the reopened session receives it through the provider's initial-prompt transport,
// exactly as a fresh `ask` does, so the same delivery evidence applies.
#[derive(Debug)]
pub(super) struct ResumePlan {
    pub(super) arguments: Vec<OsString>,
    pub(super) initial_prompt: InitialPromptDisposition,
    pub(super) watcher: Box<dyn CompletionWatcher>,
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

// The completion watcher a plan carries, before anything runs. Each adapter declares its
// own: a configuration that starts no thread and resolves nothing until `start`, so that a
// plan can be built, inspected and dropped without side effects. The shared layer knows
// only this contract and the no-op watcher of a provider whose completion arrives through
// its hook alone.
pub(super) trait CompletionWatcher: Debug {
    fn start(self: Box<Self>, directory: &Path) -> Result<Box<dyn ActiveWatcher>>;
}

// A started watcher. `stop` ends it and reports its outcome; dropping it without `stop`
// must still end it (the adapters' handles stop and join on `Drop`).
pub(super) trait ActiveWatcher {
    fn stop(self: Box<Self>) -> Result<()>;
}

// The watcher of a provider whose turn completion the hook reports by itself (Codex and
// Claude): starting and stopping it does nothing.
#[derive(Debug)]
pub(super) struct NoWatcher;

impl CompletionWatcher for NoWatcher {
    fn start(self: Box<Self>, _directory: &Path) -> Result<Box<dyn ActiveWatcher>> {
        Ok(self)
    }
}

impl ActiveWatcher for NoWatcher {
    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
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
    // Exact-workspace trust from the provider's own store. The adapter resolves the store
    // from its home in `env`; `store` is consent's recorded source store, which the recheck
    // reads in its place. The returned evidence names the store that was read.
    fn workspace_trust(
        &self,
        workspace: &Path,
        env: &super::consent::Environment,
        store: Option<&Path>,
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
    agent_bridge::supported_clis()
        .iter()
        .filter_map(|provider| launch_trust(*provider).applied_marker)
}

pub(super) fn workspace_trust(
    provider: FirstPartyCli,
    workspace: &Path,
    env: &super::consent::Environment,
    store: Option<&Path>,
) -> Result<super::consent::Trust> {
    adapter(provider).workspace_trust(workspace, env, store)
}

// The Codex memory workspace that the launch admission protects. Only the Codex adapter
// declares it; the admission applies the user's setting to it for every target provider.
pub(super) fn codex_memory_workspace(env: &super::consent::Environment) -> Result<PathBuf> {
    codex::memory_workspace(env)
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
            (FirstPartyCli::Claude, true, !cfg!(windows), None),
            (FirstPartyCli::Agy, true, false, None),
            (FirstPartyCli::Pi, false, false, Some("pi-approve-once")),
        ] {
            let facts = launch_trust(provider);
            assert_eq!(facts.answers_launch_dialog, answers, "{provider:?}");
            assert_eq!(facts.bypass_suppresses_dialog, bypass, "{provider:?}");
            assert_eq!(facts.applied_marker, marker, "{provider:?}");
            assert_eq!(
                facts.dialog_settle,
                if provider == FirstPartyCli::Claude && cfg!(windows) {
                    std::time::Duration::from_secs(1)
                } else {
                    std::time::Duration::ZERO
                },
                "{provider:?}",
            );
        }
    }
}
