use self::delivery::{Console, Io, Runtime, deliver_terminal_turn};
use self::diagnostics::{input_receipt_check, tool_confirmation_check, workspace_trust_check};
use self::readiness::ReadinessTiming;
use crate::native::delivery as native_delivery;
use crate::native::delivery::{
    Outcome, Prepared, Readiness, SurfaceInput, Transport, TransportFacts, TurnContext,
};
use crate::native::provider::{
    ActiveWatcher, CompletionWatcher, InitialPromptDisposition, LaunchContext, LaunchPlan,
    NativeProviderAdapter, ReopenSupport,
};
use crate::native::session::{Reader, Store, turn};
use crate::native::terminal;
use agent_bridge::{
    FirstPartyCli, provider_effort_args, provider_launch_args, provider_model_args,
};
use anyhow::{Context, Result, bail};
use monitor::AgyMonitor;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod delivery;
mod diagnostics;
mod log;
mod monitor;
mod readiness;
mod receipt;
mod trust;

#[cfg(test)]
mod test_support;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;

// Agy's home: its settings, which hold the trust store, under the user home. The brain
// root beside them is resolved by the monitor with its own requirement.
#[derive(Debug)]
struct Home {
    settings: PathBuf,
}

impl Home {
    fn resolve(env: &super::super::consent::Environment) -> Result<Self> {
        let home = super::super::consent::user_home(env)?;
        Ok(Self {
            settings: PathBuf::from(home).join(".gemini/antigravity-cli/settings.json"),
        })
    }
}

pub(super) struct AgyAdapter;

const AGY_TOOL_CONFIRMATION_OBSERVED: &str = "agy_tool_confirmation_observed";

const PENDING_TURN_FILE: &str = "agy-pending-turn.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const FULL_TRANSCRIPT_FILE: &str = "transcript_full.jsonl";

// Accept only the known model-family footer after the exact trust dialog; other trailing text
// leaves it to the user. See docs/verification/2026-10-01-bug-triage.md.
const AGY_MODEL_LABEL_PREFIXES: [&str; 3] = ["Gemini ", "Claude ", "GPT-"];

// Without a process-local trust API, answer only the exact managed dialog and verify the saved decision.
// Remove this fallback when Agy provides an official trust input API.
fn agy_trust_prompt_key(screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
    let lines: Vec<_> = screen
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let start = lines.iter().rposition(|s| *s == "Accessing workspace:")?;
    let lines = &lines[start..];
    let key = super::super::consent::native_key(workspace).ok()?;
    if lines.len() < 7
        || lines[1] != key
        || lines[2] != "Do you trust the contents of this project?"
        || lines[3] != "Antigravity CLI requires permission to read, edit, and execute files here."
        || lines[4] != "> Yes, I trust this folder"
        || lines[5] != "No, exit"
        || lines[6] != "↑/↓ Navigate · enter Confirm"
        || lines[7..].iter().any(|line| {
            !AGY_MODEL_LABEL_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
    {
        return None;
    }
    Some(terminal::DialogKey::Enter)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingAgyTurn {
    schema: u32,
    claim_token: String,
    marker: String,
}

impl PendingAgyTurn {
    fn new(claim_token: &str) -> Result<Self> {
        validate_claim_token(claim_token)?;
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            marker: format!("<!-- agent-bridge-agy-turn:{claim_token} -->"),
        })
    }
}

// Agy 1.2.10 writes a presence lock for every conversation but no running process holds it,
// so presence cannot prove ownership, and the transcript monitor binds only to a newly
// created conversation. Replace this refusal when Agy exposes a live-session registry or
// held lock and a resumable transcript marker.
const AGY_REOPEN_UNSUPPORTED: &str = "reopen unsupported: Agy exposes no verifiable ownership evidence for a conversation (presence locks are not held by the running process) and its transcript monitor binds only to a newly created conversation";

fn send_initial_prompt<R: Runtime>(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    runtime: &mut R,
) -> terminal::TerminalSendResult {
    if cfg!(windows) {
        deliver_terminal_turn(
            session,
            prompt_path,
            deadline,
            Some(WINDOWS_STARTUP_READINESS_TIMING),
            runtime,
        )
    } else {
        // Unreachable while the initial Transport is `LAUNCH_ARGUMENT` off
        // Windows; kept as the plain paste so the transport decision stays in one
        // place.
        runtime.split().1.send(session, prompt_path, deadline)
    }
}

fn terminal_initial_prompt(directory: &Path, prompt: &str) -> Result<String> {
    let pending =
        read_pending_turn(directory)?.context("Agy initial turn correlation state is missing")?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

fn send_terminal_follow_up<R: Runtime>(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    runtime: &mut R,
) -> terminal::TerminalSendResult {
    if cfg!(windows) {
        // The Windows initial paste already waited for startup readiness, so a
        // follow-up only takes its receipt offset and pastes.
        deliver_terminal_turn(session, prompt_path, deadline, None, runtime)
    } else {
        // Argument prompts run behind the trust dialog; each macOS follow-up needs trust, readiness
        // and a receipt before it can count as delivered. See AGENTS.md, Current Agy Boundary.
        deliver_terminal_turn(
            session,
            prompt_path,
            deadline,
            Some(MACOS_FOLLOW_UP_READINESS_TIMING),
            runtime,
        )
    }
}

fn prepare_terminal_follow_up(directory: &Path, prompt: &str, claim_token: &str) -> Result<String> {
    let pending = install_pending_turn(directory, claim_token)?;
    terminal_correlated_prompt(prompt, &pending, cfg!(windows))
}

struct AgyInitialPaste<R = Console> {
    runtime: RefCell<R>,
}
impl AgyInitialPaste<Console> {
    fn new() -> Self {
        Self {
            runtime: RefCell::new(Console::new()),
        }
    }
}
impl<R: Runtime> Transport for AgyInitialPaste<R> {
    fn facts(&self) -> TransportFacts {
        TransportFacts {
            name: "terminal-paste-fallback",
            readiness: Readiness::AfterLaunch {
                ready_delay: Duration::from_secs(0),
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
            &mut *self.runtime.borrow_mut(),
        )
        .into()
    }
    fn discard_prepared(&self, _: &TurnContext<'_>, _: Option<&Prepared>) {}
}

// Replace this paste fallback when Agy exposes addressed interactive input.
struct AgyPaste<R = Console> {
    runtime: RefCell<R>,
}
impl AgyPaste<Console> {
    fn new() -> Self {
        Self {
            runtime: RefCell::new(Console::new()),
        }
    }
}
impl<R: Runtime> Transport for AgyPaste<R> {
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
            &mut *self.runtime.borrow_mut(),
        )
        .into()
    }
    fn discard_prepared(&self, turn: &TurnContext<'_>, _: Option<&Prepared>) {
        let _ = cancel_pending_turn(turn.directory, turn.claim_token);
    }
}

impl NativeProviderAdapter for AgyAdapter {
    fn result_timeout_diagnostic(
        &self,
    ) -> Option<super::super::diagnostics::ResultTimeoutDiagnostic> {
        Some(super::super::diagnostics::ResultTimeoutDiagnostic {
            probe: false,
            detail: |check, request_id| {
                (check.reason_code.as_deref() == Some(AGY_TOOL_CONFIRMATION_OBSERVED)
                    && check.request_id.as_deref() == Some(request_id))
                .then_some(check.detail.as_deref())
                .flatten()
            },
        })
    }

    fn launch_trust(&self) -> crate::native::consent::LaunchTrust {
        crate::native::consent::LaunchTrust {
            answers_launch_dialog: true,
            bypass_suppresses_dialog: false,
            applied_marker: None,
            dialog_settle: std::time::Duration::ZERO,
        }
    }

    fn initial_transport(&self) -> Box<dyn Transport> {
        initial_transport_for_platform(cfg!(windows))
    }
    fn follow_up_transport(&self) -> Box<dyn Transport> {
        Box::new(AgyPaste::new())
    }

    fn cancel_support(&self, _directory: &Path) -> Result<super::CancelSupport> {
        Ok(super::CancelSupport::Unsupported(
            "in this release the Bridge integration for agy does not support cancel".to_owned(),
        ))
    }

    fn workspace_trust_key(&self, screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
        agy_trust_prompt_key(screen, workspace)
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
            None => Home::resolve(env)?.settings,
        };
        let Some(text) = consent::read_store(&store)? else {
            return Ok(Trust::Absent);
        };
        let config: serde_json::Value = serde_json::from_str(&text)?;
        let config = config
            .as_object()
            .context("Agy settings is not an object")?;
        let Some(entries) = config.get("trustedWorkspaces") else {
            return Ok(Trust::Absent);
        };
        let entries = entries
            .as_array()
            .context("Agy trustedWorkspaces is not an array")?;
        let key = consent::native_key(workspace)?;
        let entries = entries
            .iter()
            .map(|v| v.as_str().context("unknown Agy trustedWorkspaces entry"))
            .collect::<Result<Vec<_>>>()?;
        Ok(if entries.contains(&key.as_str()) {
            Trust::Trusted(Evidence {
                provider: "agy".into(),
                store,
                key,
            })
        } else {
            Trust::Absent
        })
    }

    fn probe_environment_removals(&self) -> &'static [&'static str] {
        // Agy derives no session identity from the caller's environment.
        &[]
    }

    fn diagnose(
        &self,
        context: super::super::diagnostics::Context<'_>,
    ) -> Vec<super::super::diagnostics::Check> {
        use super::super::diagnostics::{Availability::Unknown, Check};
        let mut checks = vec![
            Check::new(
                "agy_follow_up",
                Unknown,
                "agy_terminal_fallback",
                "Agy owns terminal-paste follow-up and transcript result monitoring. No verified first-party input path into a running interactive session is integrated. Every paste (the Windows initial prompt and every follow-up on Windows and macOS) waits for Agy's own log to show readiness and requires a HandleUserInput receipt; the Windows initial prompt and every macOS follow-up also wait until Agy's own trust store lists the exact workspace and the session's own log shows that it loaded the workspace customizations. The macOS initial prompt is a launch argument and never waits.",
                "Inspect the managed owner and terminal; retain this fallback until Agy offers a verified native path.",
            ),
            input_receipt_check(context.directory),
        ];
        // Session-scoped: only a launched session has a managed terminal that can
        // still show the trust dialog.
        if let Some(directory) = context.directory {
            checks.push(workspace_trust_check(
                context.workspace,
                context.environment,
                directory,
            ));
            checks.push(tool_confirmation_check(directory, context.environment));
        }
        checks
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        prepare_launch_for_platform(context, cfg!(windows))
    }

    fn reopen(&self) -> Result<&'static dyn ReopenSupport> {
        bail!("{AGY_REOPEN_UNSUPPORTED}")
    }

    fn handle_hook(&self, _directory: &Path, _payload: &serde_json::Value) -> Result<()> {
        bail!("Agy does not use native result hooks")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Agy does not expose Agent Bridge provider controls")
    }
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if !turn::valid_turn_claim_token(claim_token) {
        bail!("invalid Agy turn claim token")
    }
    Ok(())
}

fn initial_transport_for_platform(windows: bool) -> Box<dyn Transport> {
    if windows {
        Box::new(AgyInitialPaste::new())
    } else {
        Box::new(native_delivery::LAUNCH_ARGUMENT)
    }
}

// The complete launch command: the requested policy in Agy's flags (the bypass only when
// requested, then effort, then model), the private log file, and off Windows the
// correlated prompt as the interactive prompt argument.
fn prepare_launch_for_platform(context: LaunchContext<'_>, windows: bool) -> Result<LaunchPlan> {
    let claim_token = turn::current_claim_token(&Reader::open_unchecked(context.directory))?
        .context("Agy launch has no native turn claim")?;
    let pending = install_pending_turn(context.directory, &claim_token)?;
    let log_path = Reader::open_unchecked(context.directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let mut arguments = provider_launch_args(FirstPartyCli::Agy, context.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = context.effort {
        arguments.extend(
            provider_effort_args(FirstPartyCli::Agy, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = context.model {
        arguments.extend(
            provider_model_args(FirstPartyCli::Agy, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    arguments.extend([
        OsString::from("--log-file"),
        log_path.as_os_str().to_owned(),
    ]);
    if !windows {
        arguments.extend([
            OsString::from("--prompt-interactive"),
            OsString::from(correlated_prompt(context.prompt, &pending)),
        ]);
    }
    Ok(LaunchPlan {
        arguments,
        initial_prompt: InitialPromptDisposition::of(
            initial_transport_for_platform(windows).as_ref(),
        ),
        watcher: Box::new(AgyTranscriptWatcher { log_path }),
        environment_removals: &[],
    })
}

// The unstarted transcript watcher a launch plan carries: Agy's own log file of the
// session, which the monitor follows to the conversation's transcript. Nothing runs and
// no brain root is resolved until `start`. Replace transcript polling when Agy exposes a
// first-party per-turn completion callback with session and turn identity.
#[derive(Debug)]
struct AgyTranscriptWatcher {
    log_path: PathBuf,
}

impl CompletionWatcher for AgyTranscriptWatcher {
    fn start(self: Box<Self>, directory: &Path) -> Result<Box<dyn ActiveWatcher>> {
        Ok(Box::new(AgyMonitor::start(directory, &self.log_path)?))
    }
}

impl ActiveWatcher for AgyMonitor {
    fn stop(self: Box<Self>) -> Result<()> {
        AgyMonitor::stop(*self)
    }
}

fn install_pending_turn(directory: &Path, claim_token: &str) -> Result<PendingAgyTurn> {
    let pending = PendingAgyTurn::new(claim_token)?;
    Store::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .write_json(&pending)?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingAgyTurn>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(PENDING_TURN_FILE)
        .text()?
    else {
        return Ok(None);
    };
    let pending: PendingAgyTurn =
        serde_json::from_str(&text).context("failed to parse the pending Agy turn")?;
    let expected = PendingAgyTurn::new(&pending.claim_token)?;
    if pending.schema != expected.schema || pending.marker != expected.marker {
        bail!("Agent Bridge rejected invalid Agy turn correlation state")
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
        .private(PENDING_TURN_FILE)
        .remove()
}

fn correlated_prompt(prompt: &str, pending: &PendingAgyTurn) -> String {
    format!(
        "{prompt}\n\n[Agent Bridge Agy turn protocol]\nComplete this request as one turn. End the complete final response with the exact marker below on its own final line; do not alter or omit it.\n{}",
        pending.marker
    )
}

fn terminal_correlated_prompt(
    prompt: &str,
    pending: &PendingAgyTurn,
    windows: bool,
) -> Result<String> {
    if !windows {
        return Ok(correlated_prompt(prompt, pending));
    }
    let encoded = serde_json::to_string(prompt)?;
    Ok(format!(
        "[Agent Bridge Agy Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs. Complete it as one turn. End the complete final response with the exact marker {} on its own final line; do not alter or omit it. Request JSON: {encoded}",
        pending.marker
    ))
}

fn correlated_response<'a>(message: &'a str, pending: &PendingAgyTurn) -> Result<&'a str> {
    let body = message
        .trim_end()
        .strip_suffix(&pending.marker)
        .context("Agy response did not end with the expected turn marker")?
        .trim_end();
    if body.is_empty() {
        bail!("Agy correlated response contained no assistant text")
    }
    Ok(body)
}

const AGY_LOG_FILE: &str = "agy.log";

const HOOKS_LOADED_SOURCE: &str = "hooks_manager.go";

// Observed post-login reload bursts arrive about 3.0 seconds apart; the quiet period
// must outlast that cadence so the paste does not land between two of them.
const STARTUP_QUIET_PERIOD: Duration = Duration::from_millis(3500);

// After workspace trust, require startup, a Windows console redraw, and the quiet
// period. See AGENTS.md's Current Agy Boundary for the trust and readiness evidence.
const WINDOWS_STARTUP_READINESS_TIMING: ReadinessTiming = ReadinessTiming {
    quiet_period: STARTUP_QUIET_PERIOD,
    redraw_required: true,
};
// The macOS follow-up uses the same quiet period but no redraw marker, which Agy
// never logs on macOS. Workspace trust is checked before this gate.
const MACOS_FOLLOW_UP_READINESS_TIMING: ReadinessTiming = ReadinessTiming {
    quiet_period: STARTUP_QUIET_PERIOD,
    redraw_required: false,
};
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
// Agy processes pasted input late under load; cap receipt waiting by the caller's deadline.
// Missing receipts stay uncertain; see docs/verification/2026-10-01-bug-triage.md.
const INPUT_RECEIPT_WINDOW: Duration = Duration::from_secs(60);

const TRUST_STORE_MISSING: &str = "Agy's trust store does not list the exact workspace, and Agy keeps its trust dialog over the composer until such a workspace is approved";

const TRUST_RECOVERY: &str = "If the managed terminal shows the trust dialog, approve it there and send the prompt again; if it shows the composer or the prompt is still withheld afterwards, close this session and start a new one";

#[cfg(test)]
mod tests;

#[cfg(test)]
mod transport_tests;
