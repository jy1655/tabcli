mod credentials;
mod hook;
mod input;
mod monitor;
mod startup;
mod startup_ready;
#[cfg(test)]
mod test_support;
mod usage;
use super::super::terminal;
use super::{
    ActiveWatcher, CompletionWatcher, InitialPromptDisposition, LaunchContext, LaunchPlan,
    NativeProviderAdapter, ReopenSupport,
};
use crate::native::delivery::{
    self, Outcome, Prepared, Readiness, SurfaceInput, Transport, TransportFacts, TurnContext,
};
use crate::native::session::turn;
use crate::native::session::{Reader, RecordStore, Store};
use agent_bridge::{
    FirstPartyCli, provider_effort_args, provider_launch_args, provider_model_args,
};
use anyhow::{Context, Result, bail};
use credentials::credential_diagnosis;
use hook::{pi_owned_string, pi_string, record_correlated_failure};
use monitor::PiFailureMonitor;
use serde::{Deserialize, Serialize};
use startup::send_initial_prompt;
use startup_ready::StartupReadiness;
use std::{cell::RefCell, ffi::OsString, path::Path, time::Duration};
use usage::reported_usage;

pub(super) static ADAPTER: PiAdapter = PiAdapter;

pub(super) struct PiAdapter;

const PI_PROVIDER_CREDENTIALS: &str = "pi_provider_credentials";

const PENDING_TURN_FILE: &str = "pi-pending-turn.json";

const CANCEL_ACCEPTED_FILE: &str = "pi-cancel-accepted.json";

#[derive(Debug, Deserialize, Serialize)]
struct PendingPiTurn {
    schema: u32,
    claim_token: String,
    marker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_input: Option<input::Fingerprint>,
}

impl PendingPiTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-pi-turn:{claim_token} -->"),
            windows_input: None,
        })
    }
}

// Pi 0.84.4 keeps only transcript files under ~/.pi; no lock, pid, or registry identifies
// a live writer, and a transcript-based heuristic is not ownership evidence. Replace this
// refusal when Pi exposes a live-session registry or a held session lock.
const PI_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Pi exposes no verifiable ownership evidence for a session (no lock, pid, or registry under ~/.pi identifies a live writer)";

#[cfg(any(not(windows), test))]
fn terminal_initial_prompt(directory: &Path, prompt: &str) -> Result<String> {
    let pending =
        read_pending_turn(directory)?.context("Pi initial turn correlation state is missing")?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

#[cfg(not(windows))]
fn prepare_terminal_follow_up(directory: &Path, prompt: &str, claim_token: &str) -> Result<String> {
    let pending = install_pending_turn(directory, claim_token)?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

struct PiInitialPaste<R = input::Console> {
    input: RefCell<R>,
}

impl PiInitialPaste {
    fn new() -> Self {
        Self {
            input: RefCell::new(input::Console::default()),
        }
    }
}

impl<R: input::Runtime> Transport for PiInitialPaste<R> {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "terminal-paste-fallback",
            readiness: Readiness::AfterLaunch {
                ready_delay: Duration::from_secs(2),
            },
            surface: Some(SurfaceInput {
                submit_count: 1,
                newline: terminal::ConsoleNewline::LineFeed,
            }),
        }
    }
    fn prepare(&self, turn: &TurnContext<'_>) -> Result<Prepared> {
        Ok(Prepared {
            #[cfg(windows)]
            prompt: input::prepare_windows(turn, true)?,
            #[cfg(not(windows))]
            prompt: terminal_initial_prompt(turn.directory, turn.prompt)?,
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, prepared: &Prepared, file: Option<&Path>) -> Outcome {
        send_initial_prompt(
            turn,
            prepared,
            file.expect("surface transport has a prompt file"),
            &mut *self.input.borrow_mut(),
        )
        .into()
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

// Replace this paste fallback when Pi exposes addressed interactive input.
struct PiPaste<R = input::Console> {
    input: RefCell<R>,
}

impl PiPaste {
    fn new() -> Self {
        Self {
            input: RefCell::new(input::Console::default()),
        }
    }
}

impl<R: input::Runtime> Transport for PiPaste<R> {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "terminal-paste-fallback",
            readiness: Readiness::AfterLaunch {
                ready_delay: Duration::ZERO,
            },
            surface: Some(SurfaceInput {
                submit_count: 1,
                newline: terminal::ConsoleNewline::LineFeed,
            }),
        }
    }
    fn prepare(&self, turn: &TurnContext<'_>) -> Result<Prepared> {
        Ok(Prepared {
            #[cfg(windows)]
            prompt: input::prepare_windows(turn, false)?,
            #[cfg(not(windows))]
            prompt: prepare_terminal_follow_up(turn.directory, turn.prompt, turn.claim_token)?,
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, prepared: &Prepared, file: Option<&Path>) -> Outcome {
        #[cfg(windows)]
        return input::send_windows(
            turn,
            prepared,
            file.expect("surface transport has a prompt file"),
            &mut *self.input.borrow_mut(),
        )
        .into();
        #[cfg(not(windows))]
        {
            let _ = prepared;
            self.input
                .borrow_mut()
                .send(turn, file.expect("surface transport has a prompt file"))
                .into()
        }
    }
    fn discard_prepared(&self, turn: &TurnContext<'_>, _: Option<&Prepared>) {
        let _ = cancel_pending_turn(turn.directory, turn.claim_token);
    }
}

const APPLIED_MARKER: &str = "pi-approve-once";

// Pi's home: `trust.json` under `PI_CODING_AGENT_DIR`, or under `.pi/agent` in the user
// home. An empty override keeps the relative spelling, and the user home is required
// even when the override is set.
#[derive(Debug)]
struct Home {
    trust_store: std::path::PathBuf,
}

impl Home {
    fn resolve(env: &super::super::consent::Environment) -> Result<Self> {
        let home = super::super::consent::user_home(env)?;
        Ok(Self {
            trust_store: env
                .pi_coding_agent_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(home).join(".pi/agent"))
                .join("trust.json"),
        })
    }
}

impl NativeProviderAdapter for PiAdapter {
    fn result_timeout_diagnostic(
        &self,
    ) -> Option<super::super::diagnostics::ResultTimeoutDiagnostic> {
        Some(super::super::diagnostics::ResultTimeoutDiagnostic {
            probe: true,
            // Credential configuration belongs to the session, not a submitted request.
            detail: |check, _request_id| {
                (check.id.as_deref() == Some(PI_PROVIDER_CREDENTIALS))
                    .then_some(check.detail.as_deref())
                    .flatten()
            },
        })
    }

    fn launch_trust(&self) -> crate::native::consent::LaunchTrust {
        crate::native::consent::LaunchTrust {
            answers_launch_dialog: false,
            bypass_suppresses_dialog: false,
            applied_marker: Some(APPLIED_MARKER),
            dialog_settle: std::time::Duration::ZERO,
        }
    }

    fn initial_transport(&self) -> Box<dyn Transport> {
        initial_transport_for_platform(cfg!(windows))
    }
    fn follow_up_transport(&self) -> Box<dyn Transport> {
        Box::new(PiPaste::new())
    }

    fn cancel_support(&self, directory: &Path) -> Result<super::CancelSupport> {
        let reader = Reader::open_unchecked(directory);
        // A legacy record is unsupported before the Claim is read, as before this module.
        if let StartupReadiness::Present(ready) = startup_ready::read(&reader)
            && ready.offers_cancel()
            && turn::current_claim_token(&reader)?
                .is_some_and(|claim| ready.supports_cancel_for(&claim))
        {
            return Ok(super::CancelSupport::Supported);
        }
        Ok(super::CancelSupport::Unsupported(
            "this session was started by an extension without cancel support; start a new session"
                .to_owned(),
        ))
    }

    fn workspace_trust_key(&self, _screen: &str, _workspace: &Path) -> Option<terminal::DialogKey> {
        None
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        env: &super::super::consent::Environment,
        store: Option<&Path>,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let store = match store {
            Some(store) => store.to_path_buf(),
            None => Home::resolve(env)?.trust_store,
        };
        let Some(text) = consent::read_store(&store)? else {
            return Ok(Trust::Absent);
        };
        let config: serde_json::Value = serde_json::from_str(&text)?;
        let entries = config
            .as_object()
            .context("Pi trust store is not an object")?;
        let key = consent::native_key(workspace)?;
        match entries.get(&key) {
            Some(serde_json::Value::Bool(true)) => Ok(Trust::Trusted(Evidence {
                provider: "pi".into(),
                store,
                key,
            })),
            Some(serde_json::Value::Bool(false)) => Ok(Trust::Declined),
            None => {
                // A parent's positive decision is not exact-workspace consent; a
                // nearer negative decision still prevents overriding Pi's refusal.
                for parent in workspace.ancestors().skip(1) {
                    match entries.get(&consent::native_key(parent)?) {
                        Some(serde_json::Value::Bool(false)) => return Ok(Trust::Declined),
                        Some(serde_json::Value::Bool(true)) => break,
                        Some(_) => bail!("unknown Pi ancestor trust value"),
                        None => {}
                    }
                }
                Ok(Trust::Absent)
            }
            _ => bail!("unknown Pi trust value"),
        }
    }

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // Pi derives no session identity from the caller's environment.
        &[]
    }

    fn diagnose(
        &self,
        context: super::super::diagnostics::Context<'_>,
    ) -> Vec<super::super::diagnostics::Check> {
        use super::super::diagnostics::{Availability::Unknown, Check};
        vec![
            Check::new(
                "pi_follow_up",
                Unknown,
                "pi_terminal_fallback",
                "Pi owns terminal-paste follow-up and a session completion extension. No verified first-party external input path into a running interactive session is integrated.",
                "Inspect the managed owner and terminal. Live input was not tested; replace this fallback when Pi provides the required native path.",
            ),
            credential_diagnosis(context, |path, args, workspace, deadline| {
                super::super::doctor::probe(path, args, Some(workspace), &[], deadline)
            }),
        ]
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        prepare_launch_for_platform(context, cfg!(windows), super::super::consent::authorized)
    }

    fn reopen(&self) -> Result<&'static dyn ReopenSupport> {
        bail!("{PI_REOPEN_UNSUPPORTED}")
    }

    fn handle_hook(&self, directory: &Path, payload: &serde_json::Value) -> Result<()> {
        let Some(pending) = read_pending_turn(directory)? else {
            return Ok(());
        };
        if pi_string(payload, "agent_bridge_claim_token") != Some(pending.claim_token.as_str()) {
            return Ok(());
        }
        if payload
            .get("agent_bridge_prompt_correlated")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Ok(());
        }
        let provider_session_id = pi_owned_string(payload, "session_id");
        let turn_id = pi_owned_string(payload, "turn_id");
        // Usage is information beside the outcome; a malformed usage never changes it.
        let usage = reported_usage(payload);
        if let Some(error) = pi_string(payload, "agent_bridge_error")
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            if pi_string(payload, "agent_bridge_cancel_claim_token")
                == Some(pending.claim_token.as_str())
            {
                // Report rechecks cancel.json under the lifecycle lock. Missing,
                // damaged or different-claim intent remains an ordinary failure.
                return turn::Report::for_claim(
                    &Store::open_unchecked(directory),
                    FirstPartyCli::Pi,
                    Some(&pending.claim_token),
                )
                .with_usage(usage.usage, usage.usage_error)
                .cancelled(error, provider_session_id, turn_id)
                .context("failed to record the correlated Pi interruption");
            }
            return record_correlated_failure(
                directory,
                error,
                provider_session_id,
                turn_id,
                &pending,
                usage,
            );
        }
        let raw_message = pi_string(payload, "last_assistant_message")
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .context("Pi hook payload has no assistant result")?;
        let Ok(message) = correlated_response(raw_message, &pending) else {
            return Ok(());
        };
        turn::Report::for_claim(
            &Store::open_unchecked(directory),
            FirstPartyCli::Pi,
            Some(&pending.claim_token),
        )
        .with_usage(usage.usage, usage.usage_error)
        .complete(message, provider_session_id, turn_id)
        .context("failed to record the correlated Pi result")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Pi does not expose Agent Bridge provider controls")
    }
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if !turn::valid_turn_claim_token(claim_token) {
        bail!("invalid Pi turn claim token")
    }
    Ok(())
}

fn initial_transport_for_platform(windows: bool) -> Box<dyn Transport> {
    if windows {
        Box::new(PiInitialPaste::new())
    } else {
        Box::new(delivery::LAUNCH_ARGUMENT)
    }
}

// The complete launch command: the requested policy in Pi's flags (`--approve` only when
// the bypass is requested, then thinking, then model), the result extension, the managed
// name, one more `--approve` when consent authorizes the exact workspace, and off Windows
// the correlated prompt as the positional argument. `authorized` is the consent gate;
// production passes `consent::authorized`, and the applied marker is recorded right after
// a positive answer, before the plan exists.
fn prepare_launch_for_platform(
    context: LaunchContext<'_>,
    windows: bool,
    authorized: fn(&Path, FirstPartyCli) -> Result<bool>,
) -> Result<LaunchPlan> {
    let claim_token = turn::current_claim_token(&Reader::open_unchecked(context.directory))?
        .context("Pi launch has no native turn claim")?;
    let pending = install_pending_turn(context.directory, &claim_token)?;
    let extension_path = context.directory.join("pi-agent-bridge.js");
    RecordStore::at(&extension_path).write_private(bridge_extension().as_bytes())?;
    let mut arguments = provider_launch_args(FirstPartyCli::Pi, context.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = context.effort {
        arguments.extend(
            provider_effort_args(FirstPartyCli::Pi, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = context.model {
        arguments.extend(
            provider_model_args(FirstPartyCli::Pi, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    arguments.extend([
        OsString::from("--extension"),
        extension_path.into_os_string(),
        OsString::from("--name"),
        OsString::from(context.title),
    ]);
    if authorized(context.directory, FirstPartyCli::Pi)? {
        // Pi documents --approve as a one-run project-trust override. It is
        // not a tool permission flag and does not persist to trust.json.
        arguments.push(OsString::from("--approve"));
        super::super::consent::applied(context.directory, APPLIED_MARKER)?;
    }
    if !windows {
        arguments.push(OsString::from(correlated_prompt(context.prompt, &pending)));
    }
    Ok(LaunchPlan {
        arguments,
        initial_prompt: InitialPromptDisposition::of(
            initial_transport_for_platform(windows).as_ref(),
        ),
        watcher: Box::new(PiHookFailureWatcher),
        environment_removals: &[],
    })
}

// The unstarted hook-failure watcher a launch plan carries; no thread runs until `start`.
// Replace the local lifecycle extension when Pi exposes a first-party external completion
// callback with turn identity.
#[derive(Debug)]
struct PiHookFailureWatcher;

impl CompletionWatcher for PiHookFailureWatcher {
    fn start(self: Box<Self>, directory: &Path) -> Result<Box<dyn ActiveWatcher>> {
        Ok(Box::new(PiFailureMonitor::start(directory)?))
    }
}

impl ActiveWatcher for PiFailureMonitor {
    fn stop(self: Box<Self>) -> Result<()> {
        PiFailureMonitor::stop(*self)
    }
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingPiTurn> {
    install_pending_turn_bound(directory, claim_token, None)
}

fn install_pending_turn_bound(
    directory: &Path,
    claim_token: &str,
    windows_input: Option<input::Fingerprint>,
) -> Result<PendingPiTurn> {
    let mut pending = PendingPiTurn::new(claim_token)?;
    pending.windows_input = windows_input;
    let store = Store::open_unchecked(directory);
    let result = (turn::ClaimCurrency {
        expected: Some(claim_token),
        state: turn::ClaimState::Any,
        tombstone: turn::ClaimTombstone::Ignored,
        expires: None,
    })
    .run(&store, || {
        store.private(CANCEL_ACCEPTED_FILE).remove()?;
        // Carry session startup capability to this claim without upgrading legacy evidence.
        startup_ready::carry_forward(&store, claim_token)?;
        store.private(PENDING_TURN_FILE).write_json(&pending)?;
        Ok(pending)
    })?;
    match result {
        Ok(pending) => Ok(pending),
        Err(_) => bail!("Pi pending turn no longer holds the session claim"),
    }
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingPiTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .timeline(4096)?
    else {
        return Ok(None);
    };
    let pending: PendingPiTurn =
        serde_json::from_slice(&text).context("failed to parse the pending Pi turn")?;
    let expected = PendingPiTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Pi turn correlation state")
    }
    if let Some(fingerprint) = &pending.windows_input {
        fingerprint.validate()?;
    }
    Ok(Some(pending))
}

fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let store = Store::open_unchecked(directory);
    let _lock = store.lock()?;
    if (turn::ClaimCurrency {
        expected: Some(claim_token),
        state: turn::ClaimState::Any,
        tombstone: turn::ClaimTombstone::Ignored,
        expires: None,
    })
    .check(&store)?
    .is_err()
    {
        return Ok(());
    }
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    store.private(CANCEL_ACCEPTED_FILE).remove()?;
    store.private(PENDING_TURN_FILE).remove()
}

fn correlated_prompt(prompt: &str, pending: &PendingPiTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Pi turn protocol]\nCorrelation marker for the Agent Bridge extension:\n{}",
        pending.marker
    )
}

fn terminal_correlated_prompt(
    prompt: &str,
    pending: &PendingPiTurn,
    windows: bool,
) -> Result<String> {
    if !windows {
        return Ok(correlated_prompt(prompt, pending));
    }
    let encoded = serde_json::to_string(prompt)?;
    Ok(format!(
        "[Agent Bridge Pi Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs, and complete it as one turn. Request JSON: {encoded} Correlation marker for the Agent Bridge extension: {}",
        pending.marker
    ))
}

fn correlated_response<'a>(message: &'a str, pending: &PendingPiTurn) -> Result<&'a str> {
    let message = message.trim_end();
    let body = message
        .strip_suffix(&pending.marker)
        .unwrap_or(message)
        .trim_end();
    if body.is_empty() {
        bail!("Pi correlated response contained no assistant text")
    }
    Ok(body)
}

fn bridge_extension() -> &'static str {
    include_str!("../pi_extension.js")
}

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod tests;
