mod agy;
mod capped_output;
// Named from `crate::native` so that the reopen tests can point the Claude session-registry
// reads at a fixture through the Claude module itself (`override_session_registry_for_test`).
pub(super) mod claude;
mod codex;
mod pi;
// Shared tests write a Claude session's completion records and a Codex session's launch
// decision without spelling their records.
#[cfg(test)]
pub(super) use claude::CompletionRecordFixture as ClaudeCompletionRecordFixture;
#[cfg(test)]
pub(super) use codex::LaunchDecisionFixture as CodexLaunchDecisionFixture;

use agent_bridge::FirstPartyCli;
use anyhow::Result;
use std::{ffi::OsString, fmt::Debug, path::Path, path::PathBuf, str::FromStr, time::Instant};

use super::delivery::Transport;

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub(super) struct EmittedQuestion {
    pub(super) title: String,
    pub(super) options: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub(super) struct QuestionEmission {
    pub(super) id: String,
    pub(super) questions: Vec<EmittedQuestion>,
    /// The provider's own adapter refused this call before any dialog and returned the question
    /// to the caller in the Result. Only an adapter with such a route sets it (Claude today).
    /// Omitted when false, so other records and outputs keep their bytes; old records read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(super) deflected: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    if !locked {
        return None;
    }
    adapter(FirstPartyCli::from_str(provider).ok()?).question_emissions(reader, receipt)
}

/// Provider-owned completion evidence, observed only under inspect's existing lock.
pub(super) fn completion_observation(
    reader: &super::session::Reader,
    provider: &str,
    locked: bool,
    claim: Option<&str>,
) -> Option<serde_json::Value> {
    if !locked {
        return None;
    }
    adapter(FirstPartyCli::from_str(provider).ok()?).completion_observation(reader, claim?)
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
    // Adapter-selected turn correlation value, not a TAB Request id:
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

/// What a follow-up's adapter-owned gate decides from this session's own records alone,
/// before any wait, probe or provider process. The delivery path keeps its own checks.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FollowUpStatic {
    /// The evaluated gate passes; `unevaluated` names the adapter gates inspect does not
    /// evaluate: waits, probes, and record gates left to delivery.
    Ok {
        unevaluated: &'static [&'static str],
    },
    /// The action refuses on these records; `detail` is the text it reports.
    Refused {
        reason: &'static str,
        detail: String,
    },
    /// A record the gate needs could not be read.
    Unknown(String),
}

/// Whether this session was configured to observe question emissions. Configuration is not
/// evidence that the observer loaded or ran.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EmissionObservation {
    Configured(&'static str),
    Unavailable(&'static str),
    Unsupported(&'static str),
    /// A launch record the adapter decides from could not be read.
    Unknown(String),
}

/// Whether TAB can answer an emitted question for the provider.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum QuestionResponse {
    Unsupported(&'static str),
    /// The launch configured a refusal that returns the question in the Result, answered
    /// by an ordinary `tell`. Configuration is not evidence that any call was refused.
    DeflectedToResult(&'static str),
    /// The launch configured a notice that lets the question through and tells the child that no
    /// reply can arrive during the turn; the caller answers with an ordinary `tell` after the
    /// Request has ended. Configuration is not evidence that the child read or followed it.
    AnswerAfterTurn(&'static str),
    /// A launch record the adapter decides from could not be read.
    Unknown(String),
}

/// No supported provider offers an addressed answer for an open question dialog; the
/// Claude Code 2.1.318 documentation was checked on 2026-11-05.
pub(super) const NO_ADDRESSED_ANSWER_PATH: &str = "no_addressed_answer_path";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct QuestionCapability {
    pub(super) emission: EmissionObservation,
    pub(super) response: QuestionResponse,
}

/// Read-only and lock-free; inspect calls it only under its retained Snapshot lock.
pub(super) fn follow_up_static(
    provider: FirstPartyCli,
    reader: &super::session::Reader,
) -> FollowUpStatic {
    adapter(provider).follow_up_static(reader)
}

/// Read-only and lock-free; inspect calls it only under its retained Snapshot lock.
pub(super) fn question_capability(
    provider: FirstPartyCli,
    reader: &super::session::Reader,
) -> QuestionCapability {
    adapter(provider).question_capability(reader)
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
    fn question_emissions(
        &self,
        reader: &super::session::Reader,
        receipt: &super::session::requests::Receipt,
    ) -> Option<QuestionEmissions>;
    fn completion_observation(
        &self,
        reader: &super::session::Reader,
        claim: &str,
    ) -> Option<serde_json::Value>;
    fn follow_up_transport(&self) -> Box<dyn Transport>;
    fn cancel_support(&self, directory: &Path) -> Result<CancelSupport>;
    // The follow-up gate this adapter evaluates for inspect from its own records, before any
    // wait, probe or provider process. It names every adapter gate it does not evaluate,
    // including any that delivery decides from records. Reads only; no lock.
    fn follow_up_static(&self, reader: &super::session::Reader) -> FollowUpStatic;
    // Whether question emission observation was configured for this session, and the
    // answer path. Reads only; no lock.
    fn question_capability(&self, reader: &super::session::Reader) -> QuestionCapability;
    // How this adapter treats its provider's startup workspace dialog: its policy for a
    // launch, or that it does not answer the dialog.
    fn launch_dialog(&self) -> super::consent::LaunchDialogSupport<'static>;
    // The consent applied marker this adapter records when its own launch applies workspace
    // trust, if any.
    fn applied_marker(&self) -> Option<&'static str>;
    // Exact-workspace trust from the provider's own store. The adapter resolves the store
    // from its home in `env`; `store` is consent's recorded source store, which the recheck
    // reads in its place. The returned evidence names the store that was read.
    fn workspace_trust(
        &self,
        workspace: &Path,
        env: &super::consent::Environment,
        store: Option<&Path>,
    ) -> Result<super::consent::Trust>;
    fn result_timeout_diagnostic(&self) -> Option<super::diagnostics::ResultTimeoutDiagnostic>;
    fn diagnose(&self, context: super::diagnostics::Context<'_>) -> Vec<super::diagnostics::Check>;
    // Caller-environment variables removed from every short-lived provider query that
    // TAB runs (version preflight, doctor probes). Each adapter states its own list;
    // there is no shared default.
    fn probe_environment_removals(&self) -> &'static [&'static str];
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan>;
    // Ok means an implementation exists, not availability for this platform or conversation.
    // Each operation verifies its own availability.
    fn reopen(&self) -> Result<&'static dyn ReopenSupport>;
    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()>;
    fn run_control(&self, arguments: &[String]) -> Result<()>;
    // Whether this adapter's launch and turn transports can run on a surface of `surface`
    // on this platform, decided before any session exists, and the initial-prompt Transport
    // selected for it. `version` is `None` before the CLI version has been probed (such as
    // during terminal selection, unprobed doctor checks, or early platform admission) and
    // `Some` once the installed version is known. `Err` names the capability or version
    // requirement the surface lacks for this provider. No default: each adapter states its
    // own answer and initial Transport. `Ok` is a code path, not live evidence.
    fn surface_support(
        &self,
        surface: super::terminal::TerminalKind,
        version: Option<&str>,
    ) -> SurfaceSupport;
}

/// Why an adapter refuses to run on a surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SurfaceRefusal {
    /// The adapter cannot run on this surface on this platform at all, for any version.
    Platform(&'static str),
    /// The adapter can run on this surface on this platform, but not with the probed or
    /// recorded provider version.
    Version(String),
}

impl std::fmt::Display for SurfaceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform(reason) => f.write_str(reason),
            Self::Version(reason) => f.write_str(reason),
        }
    }
}

/// An adapter's answer on a surface kind: the initial-prompt Transport for that surface
/// on this platform, or the missing capability or version requirement.
pub(super) type SurfaceSupport = std::result::Result<Box<dyn Transport>, SurfaceRefusal>;

fn adapter(provider: FirstPartyCli) -> &'static dyn NativeProviderAdapter {
    match provider {
        FirstPartyCli::Codex => &codex::ADAPTER,
        FirstPartyCli::Claude => &claude::ADAPTER,
        FirstPartyCli::Agy => &agy::ADAPTER,
        FirstPartyCli::Pi => &pi::ADAPTER,
    }
}

pub(super) fn launch_dialog(
    provider: FirstPartyCli,
) -> super::consent::LaunchDialogSupport<'static> {
    adapter(provider).launch_dialog()
}

// Native trust waits accept any adapter-declared application marker, independently
// of the target provider; authorization remains a separate required observation.
pub(super) fn launch_trust_markers() -> impl Iterator<Item = &'static str> {
    agent_bridge::supported_clis()
        .iter()
        .filter_map(|provider| adapter(*provider).applied_marker())
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

pub(super) fn diagnose(
    provider: FirstPartyCli,
    context: super::diagnostics::Context<'_>,
) -> Vec<super::diagnostics::Check> {
    adapter(provider).diagnose(context)
}

pub(super) fn result_timeout_diagnostic(
    provider: FirstPartyCli,
) -> Option<super::diagnostics::ResultTimeoutDiagnostic> {
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

/// Whether `provider`'s adapter can run on `surface` on this platform before any CLI
/// version probe. Terminal selection and unprobed diagnostics read this.
pub(super) fn surface_support(
    provider: FirstPartyCli,
    surface: super::terminal::TerminalKind,
) -> SurfaceSupport {
    adapter(provider).surface_support(surface, None)
}

/// The admission of a selected surface for a provider, before any session exists: returns
/// the adapter's selected initial-prompt Transport on admission, or its refusal as an
/// unsupported failure. Called with `None` before resolution to refuse unconditionally
/// unsupported platform/surface pairs, and again with the probed version once the installed
/// CLI version is known.
pub(super) fn admit_surface(
    provider: FirstPartyCli,
    surface: super::terminal::TerminalKind,
    version: Option<&str>,
) -> Result<Box<dyn Transport>> {
    adapter(provider)
        .surface_support(surface, version)
        .map_err(|refusal| {
            let detail = match refusal {
                SurfaceRefusal::Platform(reason) => format!(
                    "{} on {} is unsupported on this platform: {reason}",
                    provider.as_str(),
                    surface.display_name()
                ),
                SurfaceRefusal::Version(reason) => format!(
                    "{} on {} is unsupported: {reason}",
                    provider.as_str(),
                    surface.display_name()
                ),
            };
            super::failure_kind::Unsupported::new(detail).into()
        })
}

pub(super) fn follow_up_transport(provider: FirstPartyCli) -> Box<dyn Transport> {
    adapter(provider).follow_up_transport()
}

#[cfg(test)]
fn transport_test_context<'a>(
    directory: &'a Path,
    surface: &'a super::terminal::TerminalSession,
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

    // Each adapter answers for itself. Pre-probe, only Codex on Windows Herdr refuses;
    // post-probe, Codex on non-Windows Herdr also enforces stable 0.169.0+. Claude, Agy,
    // and Pi admit every surface and version unchanged. Every admitted answer carries the
    // exact initial Transport that will run on that surface.
    #[test]
    fn adapters_declare_which_surfaces_their_transports_run_on() {
        use super::super::delivery::Readiness;
        use super::super::failure_kind::{FailureKind, classify};
        use super::super::terminal::{ConsoleNewline, TerminalKind};

        for provider in agent_bridge::supported_clis().iter().copied() {
            for surface in TerminalKind::ALL {
                let windows_codex_herdr = cfg!(windows)
                    && provider == FirstPartyCli::Codex
                    && surface == TerminalKind::Herdr;
                assert_eq!(
                    surface_support(provider, surface).is_err(),
                    windows_codex_herdr,
                    "{provider:?} on {surface:?}"
                );
                match (admit_surface(provider, surface, None), windows_codex_herdr) {
                    (Ok(_), false) => {}
                    (Err(error), true) => {
                        let text = format!("{error:#}");
                        assert!(
                            text.starts_with(
                                "codex on Herdr is unsupported on this platform: Codex's Windows initial prompt is the injected console paste (#124)"
                            ),
                            "{text}"
                        );
                        assert!(
                            text.ends_with("Choose --terminal windows-console"),
                            "{text}"
                        );
                        assert_eq!(classify(&error), Some(FailureKind::Unsupported));
                    }
                    (result, refused) => {
                        panic!(
                            "{provider:?} on {surface:?} (unprobed): ok={}, expected refused={refused}",
                            result.is_ok()
                        )
                    }
                }

                for (version, codex_herdr_ok) in [
                    ("codex-cli 0.168.0", false),
                    ("codex-cli 0.169.0-alpha.1", false),
                    ("codex-cli 0.170.0-alpha.1", false),
                    ("codex-cli 0.170.0--", false),
                    ("codex-cli 0.170.0+", false),
                    ("codex-cli 0.168.0+ (runtime 22.0.0)", false),
                    ("codex-cli 0.168.0- (protocol 1.0.0)", false),
                    ("codex-cli 0.169.0", true),
                    ("codex-cli 0.170.0", true),
                    ("unknown", false),
                ] {
                    let version_refused = !cfg!(windows)
                        && provider == FirstPartyCli::Codex
                        && surface == TerminalKind::Herdr
                        && !codex_herdr_ok;
                    let result = admit_surface(provider, surface, Some(version));
                    match (result, windows_codex_herdr, version_refused) {
                        (Ok(_), false, false) => {}
                        (Err(error), true, false) => {
                            let text = format!("{error:#}");
                            assert!(
                                text.starts_with(
                                    "codex on Herdr is unsupported on this platform: Codex's Windows initial prompt is the injected console paste (#124)"
                                ),
                                "{text}"
                            );
                            assert_eq!(classify(&error), Some(FailureKind::Unsupported));
                        }
                        (Err(error), false, true) => {
                            let text = format!("{error:#}");
                            assert!(
                                text.starts_with(
                                    "codex on Herdr is unsupported: Herdr requires stable Codex 0.169.0 or newer with --no-daemon;"
                                ),
                                "{text}"
                            );
                            assert_eq!(classify(&error), Some(FailureKind::Unsupported));
                        }
                        (result, platform_refused, version_refused) => {
                            panic!(
                                "{provider:?} on {surface:?} ({version}): ok={}, expected platform_refused={platform_refused}, version_refused={version_refused}",
                                result.is_ok()
                            )
                        }
                    }
                }

                // Every admitted (provider, surface) at a supported version directly returns
                // the initial Transport that delivery will use.
                if let Ok(transport) = admit_surface(provider, surface, Some("codex-cli 0.169.0")) {
                    let facts = transport.facts();
                    match (provider, cfg!(windows)) {
                        (FirstPartyCli::Codex, true) => {
                            assert_ne!(
                                surface,
                                TerminalKind::Herdr,
                                "Codex's console-only paste transport must never be paired with an admitted Herdr surface"
                            );
                            assert_eq!(facts.name, "terminal-paste-fallback");
                            let input = facts.surface.expect("Windows Codex uses console paste");
                            assert_eq!(input.submit_count, 2);
                            assert_eq!(input.newline, ConsoleNewline::ShiftReturn);
                        }
                        (FirstPartyCli::Codex, false) => {
                            assert_eq!(facts.name, "launch-argument");
                            assert_eq!(facts.readiness, Readiness::LaunchArgument);
                            assert!(facts.surface.is_none());
                        }
                        (FirstPartyCli::Claude, _) => {
                            assert_eq!(facts.name, "provider-cross-session-message");
                            assert!(facts.surface.is_none());
                        }
                        (FirstPartyCli::Agy | FirstPartyCli::Pi, true) => {
                            assert_eq!(facts.name, "terminal-paste-fallback");
                            let input = facts.surface.expect("Windows Agy/Pi uses surface paste");
                            assert_eq!(input.submit_count, 1);
                            assert_eq!(input.newline, ConsoleNewline::LineFeed);
                        }
                        (FirstPartyCli::Agy | FirstPartyCli::Pi, false) => {
                            assert_eq!(facts.name, "launch-argument");
                            assert_eq!(facts.readiness, Readiness::LaunchArgument);
                            assert!(facts.surface.is_none());
                        }
                    }
                }
            }
        }
        assert!(
            CODEX_WINDOWS_HERDR_REFUSAL_WORDS
                .iter()
                .all(|words| codex::windows_herdr_refusal().contains(words))
        );
    }

    // The concrete facts the Codex refusal rests on, so that its text cannot drift from them.
    const CODEX_WINDOWS_HERDR_REFUSAL_WORDS: [&str; 4] = [
        "without Return",
        "only against the exact accepted screen",
        "32,767 characters",
        "8,191 and no line break through an npm codex.cmd",
    ];

    #[test]
    fn read_only_observations_dispatch_through_adapters() {
        use crate::native::session::{Reader, Store, requests::Receipt};

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        let receipt = Receipt {
            schema: 1,
            request_id: "request-1-2-3".into(),
            claim_token: "1-2-3".into(),
            event_file: "turn-1-2-3.json".into(),
            created_unix_ms: Some(1),
            source: None,
            context_sources: Vec::new(),
        };
        // Malformed private records distinguish an adapter read from an unsupported operation.
        store
            .private("codex-questions-1-2-3.json")
            .write_private(b"{")
            .unwrap();
        ClaudeCompletionRecordFixture::write_malformed_questions(directory.path(), "1-2-3");
        ClaudeCompletionRecordFixture::MALFORMED_MODE.write(directory.path());
        let reader = Reader::open_unchecked(directory.path());
        for (provider, questions, completion) in [
            (FirstPartyCli::Codex.as_str(), true, false),
            (FirstPartyCli::Claude.as_str(), true, true),
            (FirstPartyCli::Agy.as_str(), false, false),
            (FirstPartyCli::Pi.as_str(), false, false),
            ("unrecognized", false, false),
        ] {
            for locked in [true, false] {
                let observed = question_emissions(&reader, provider, locked, &receipt);
                assert_eq!(observed.is_some(), locked && questions, "{provider}");
                if let Some(observed) = observed {
                    assert!(observed.items.is_empty());
                    assert!(observed.observation == QuestionObservation::Unreadable);
                }
                let observed =
                    completion_observation(&reader, provider, locked, Some(&receipt.claim_token));
                assert_eq!(observed.is_some(), locked && completion, "{provider}");
                if let Some(observed) = observed {
                    assert_eq!(observed["state"], "unreadable");
                }
                assert!(completion_observation(&reader, provider, locked, None).is_none());
            }
        }
        // The follow-up gate and the question capability dispatch to each adapter: a
        // malformed record an adapter reads is unknown, never refused or unavailable.
        CodexLaunchDecisionFixture::LIFECYCLE_MALFORMED.write(directory.path());
        let answer = QuestionResponse::Unsupported(NO_ADDRESSED_ANSWER_PATH);
        for (provider, follow_up, emission) in [
            (
                FirstPartyCli::Codex,
                FollowUpStatic::Ok {
                    unevaluated: &["codex_native_queue_version", "codex_thread"],
                },
                false,
            ),
            (
                FirstPartyCli::Claude,
                FollowUpStatic::Unknown(String::new()),
                false,
            ),
            (
                FirstPartyCli::Agy,
                FollowUpStatic::Ok {
                    unevaluated: &["agy_trust_store", "agy_log_readiness"],
                },
                true,
            ),
            // No startup record: native Windows follow-ups refuse; elsewhere Pi has no gate.
            (
                FirstPartyCli::Pi,
                if cfg!(windows) {
                    FollowUpStatic::Refused {
                        reason: "pi_input_ack_unsupported",
                        detail: "this Pi session cannot acknowledge Windows input; start a new TAB session; no console input was sent: Pi startup receipt is missing".into(),
                    }
                } else {
                    FollowUpStatic::Ok { unevaluated: &[] }
                },
                true,
            ),
        ] {
            match (follow_up_static(provider, &reader), follow_up) {
                (FollowUpStatic::Unknown(detail), FollowUpStatic::Unknown(_)) => {
                    assert!(!detail.is_empty(), "{provider:?}");
                }
                (observed, expected) => assert_eq!(observed, expected, "{provider:?}"),
            }
            let observed = question_capability(provider, &reader);
            if provider == FirstPartyCli::Claude {
                // Claude's answer path is decided from the same malformed completion-mode record.
                assert!(
                    matches!(&observed.response, QuestionResponse::Unknown(detail) if !detail.is_empty()),
                    "{provider:?}"
                );
            } else {
                assert_eq!(observed.response, answer, "{provider:?}");
            }
            if emission {
                assert_eq!(
                    observed.emission,
                    EmissionObservation::Unsupported("no_question_plugin"),
                    "{provider:?}"
                );
            } else {
                assert!(
                    matches!(&observed.emission, EmissionObservation::Unknown(detail) if !detail.is_empty()),
                    "{provider:?}"
                );
            }
        }
    }

    #[test]
    fn launch_records_decide_the_follow_up_gate_and_question_configuration() {
        use crate::native::session::{Reader, Store};

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_unchecked(directory.path());
        let reader = Reader::open_unchecked(directory.path());
        let emission = |provider| question_capability(provider, &reader).emission;
        // No launch record: a pre-hook Codex launch and a legacy Claude completion session.
        assert_eq!(
            emission(FirstPartyCli::Codex),
            EmissionObservation::Unavailable("hooks_not_injected")
        );
        assert_eq!(
            emission(FirstPartyCli::Claude),
            EmissionObservation::Unavailable("legacy_completion_mode")
        );
        assert!(matches!(
            follow_up_static(FirstPartyCli::Claude, &reader),
            FollowUpStatic::Refused {
                reason: "claude_legacy_completion",
                ..
            }
        ));
        for (decision, expected) in [
            (
                CodexLaunchDecisionFixture::LIFECYCLE_INJECTED,
                EmissionObservation::Configured("hooks_injected_unverified"),
            ),
            (
                CodexLaunchDecisionFixture::LIFECYCLE_NOT_INJECTED,
                EmissionObservation::Unavailable("hooks_not_injected"),
            ),
        ] {
            decision.write(directory.path());
            assert_eq!(emission(FirstPartyCli::Codex), expected);
        }
        // A record Codex's own reader rejects is unknown, not a configuration.
        CodexLaunchDecisionFixture::LIFECYCLE_INVALID.write(directory.path());
        assert!(matches!(
            emission(FirstPartyCli::Codex),
            EmissionObservation::Unknown(_)
        ));
        ClaudeCompletionRecordFixture::NATIVE_MODE.write(directory.path());
        // Native mode records the plugin prepared at launch only with its manifest; a
        // v0.4.25 native launch has none.
        assert_eq!(
            emission(FirstPartyCli::Claude),
            EmissionObservation::Unavailable("question_plugin_not_prepared")
        );
        std::fs::create_dir_all(directory.path().join("claude-question-mod/.claude-plugin"))
            .unwrap();
        store
            .private("claude-question-mod/.claude-plugin/plugin.json")
            .write_private(br#"{"name":"agent-bridge-questions"}"#)
            .unwrap();
        assert_eq!(
            emission(FirstPartyCli::Claude),
            EmissionObservation::Configured("plugin_configured_unverified")
        );
        assert_eq!(
            follow_up_static(FirstPartyCli::Claude, &reader),
            FollowUpStatic::Ok {
                unevaluated: &["claude_native_readiness"],
            }
        );
    }

    #[test]
    fn adapters_declare_launch_dialog_policies() {
        use super::super::consent::{Bypass, LaunchDialogSupport, LaunchFacts, ProviderStart};
        let directory = tempfile::tempdir().unwrap();
        for (provider, answers, bypass_start, marker) in [
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
            assert_eq!(adapter(provider).applied_marker(), marker, "{provider:?}");
            let answerer = match launch_dialog(provider) {
                LaunchDialogSupport::NotAnswered => None,
                LaunchDialogSupport::Answered(answerer) => Some(answerer),
            };
            assert_eq!(answerer.is_some(), answers, "{provider:?}");
            let Some(answerer) = answerer else {
                continue;
            };
            for bypass in [Bypass::On, Bypass::Off] {
                let policy = answerer.policy(LaunchFacts {
                    directory: directory.path(),
                    workspace: directory.path(),
                    bypass,
                });
                let start = policy.bypass_start();
                assert_eq!(start.is_some(), bypass_start, "{provider:?} {bypass:?}");
                // An empty session directory has no start signal, and the unanswered deadline
                // under bypass ends with the adapter's readiness text.
                if let Some(start) = start {
                    assert_eq!(start.started().unwrap(), ProviderStart::NoSignal);
                    assert!(
                        start
                            .not_started()
                            .contains("readiness was not observed before the delivery deadline")
                    );
                }
                assert_eq!(
                    policy.settle(),
                    if provider == FirstPartyCli::Claude && cfg!(windows) {
                        std::time::Duration::from_secs(1)
                    } else {
                        std::time::Duration::ZERO
                    },
                    "{provider:?} {bypass:?}",
                );
            }
        }
    }
}
