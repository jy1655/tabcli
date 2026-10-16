mod credentials;
mod hook;
mod monitor;
mod startup;
#[cfg(test)]
mod test_support;
use super::super::terminal;
use super::{
    CompletionMonitor, InitialPromptDisposition, LaunchContext, LaunchPlan, NativeProviderAdapter,
    ReopenSupport,
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
pub(super) use monitor::PiFailureMonitor;
use serde::{Deserialize, Serialize};
use startup::send_initial_prompt;
use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

pub(super) static ADAPTER: PiAdapter = PiAdapter;

pub(super) struct PiAdapter;

const PI_PROVIDER_CREDENTIALS: &str = "pi_provider_credentials";

const PENDING_TURN_FILE: &str = "pi-pending-turn.json";

const CANCEL_ACCEPTED_FILE: &str = "pi-cancel-accepted.json";

const STARTUP_READY_FILE: &str = "pi-startup-ready.json";

#[derive(Deserialize, Serialize)]
struct StartupReady {
    schema: u32,
    claim_token: String,
    session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct PendingPiTurn {
    schema: u32,
    claim_token: String,
    marker: String,
}

impl PendingPiTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-pi-turn:{claim_token} -->"),
        })
    }
}

// Pi 0.84.4 keeps only transcript files under ~/.pi; no lock, pid, or registry identifies
// a live writer, and a transcript-based heuristic is not ownership evidence. Replace this
// refusal when Pi exposes a live-session registry or a held session lock.
const PI_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Pi exposes no verifiable ownership evidence for a session (no lock, pid, or registry under ~/.pi identifies a live writer)";

fn terminal_initial_prompt(directory: &Path, prompt: &str) -> Result<String> {
    let pending =
        read_pending_turn(directory)?.context("Pi initial turn correlation state is missing")?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

fn send_terminal_follow_up(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> terminal::TerminalSendResult {
    terminal::send_file(session, prompt_path, deadline)
}

fn prepare_terminal_follow_up(directory: &Path, prompt: &str, claim_token: &str) -> Result<String> {
    let pending = install_pending_turn(directory, claim_token)?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

struct PiInitialPaste;

impl Transport for PiInitialPaste {
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
            prompt: terminal_initial_prompt(turn.directory, turn.prompt)?,
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, _: &Prepared, file: Option<&Path>) -> Outcome {
        send_initial_prompt(
            turn.surface,
            file.expect("surface transport has a prompt file"),
            turn.deadline,
        )
        .into()
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

// Replace this paste fallback when Pi exposes addressed interactive input.
struct PiPaste;

impl Transport for PiPaste {
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
            prompt: prepare_terminal_follow_up(turn.directory, turn.prompt, turn.claim_token)?,
            correlation_id: None,
        })
    }
    fn send(&self, turn: &TurnContext<'_>, _: &Prepared, file: Option<&Path>) -> Outcome {
        send_terminal_follow_up(
            turn.surface,
            file.expect("surface transport has a prompt file"),
            turn.deadline,
        )
        .into()
    }
    fn discard_prepared(&self, turn: &TurnContext<'_>, _: Option<&Prepared>) {
        let _ = cancel_pending_turn(turn.directory, turn.claim_token);
    }
}

const APPLIED_MARKER: &str = "pi-approve-once";

impl NativeProviderAdapter for PiAdapter {
    fn result_timeout_diagnostic(&self) -> Option<super::super::doctor::ResultTimeoutDiagnostic> {
        Some(super::super::doctor::ResultTimeoutDiagnostic {
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
        }
    }

    fn initial_transport(&self) -> Box<dyn Transport> {
        initial_transport_for_platform(cfg!(windows))
    }
    fn follow_up_transport(&self) -> Box<dyn Transport> {
        Box::new(PiPaste)
    }

    fn cancel_support(&self, directory: &Path) -> Result<super::CancelSupport> {
        let reader = Reader::open_unchecked(directory);
        let ready = reader
            .private(STARTUP_READY_FILE)
            .optional_json::<StartupReady>();
        if let Ok(Some(ready)) = ready
            && ready.schema == 2
            && !ready.session_id.trim().is_empty()
            && turn::current_claim_token(&reader)?.as_deref() == Some(&ready.claim_token)
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
        homes: &super::super::consent::Homes,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let Some(text) = consent::read_store(&homes.pi)? else {
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
                store: homes.pi.clone(),
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
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        use super::super::doctor::{Availability::Unknown, Check};
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
                .cancelled(error, provider_session_id, turn_id)
                .context("failed to record the correlated Pi interruption");
            }
            return record_correlated_failure(
                directory,
                error,
                provider_session_id,
                turn_id,
                &pending,
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
        Box::new(PiInitialPaste)
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
        // Replace the local lifecycle extension when Pi exposes a
        // first-party external completion callback with turn identity.
        completion_monitor: CompletionMonitor::PiHookFailure,
        environment_removals: &[],
    })
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingPiTurn> {
    let pending = PendingPiTurn::new(claim_token)?;
    let store = Store::open_unchecked(directory);
    store.private(CANCEL_ACCEPTED_FILE).remove()?;
    // Startup proves the installed extension's capability for the whole session.
    // Carry that evidence to a follow-up claim before delivery, including after a
    // rolled-back delivery; never upgrade a legacy extension's schema.
    if let Ok(Some(mut ready)) = store
        .private(STARTUP_READY_FILE)
        .optional_json::<StartupReady>()
        && ready.schema == 2
        && validate_claim_token(&ready.claim_token).is_ok()
        && !ready.session_id.trim().is_empty()
    {
        ready.claim_token = claim_token.to_owned();
        store.private(STARTUP_READY_FILE).write_json(&ready)?;
    }
    store.private(PENDING_TURN_FILE).write_json(&pending)?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingPiTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
    else {
        return Ok(None);
    };
    let pending: PendingPiTurn =
        serde_json::from_str(&text).context("failed to parse the pending Pi turn")?;
    let expected = PendingPiTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Pi turn correlation state")
    }
    Ok(Some(pending))
}

fn cancel_pending_turn(directory: &Path, claim_token: &str) -> Result<()> {
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(());
    };
    if pending.claim_token != claim_token {
        return Ok(());
    }
    Store::open_unchecked(directory)
        .private(CANCEL_ACCEPTED_FILE)
        .remove()?;
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .remove()
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
mod tests;
