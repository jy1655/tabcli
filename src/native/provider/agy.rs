use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter, ResumeContext, ResumePlan, ResumedSessionContext,
};
use crate::native::session::SessionState;
use crate::native::session::turn;
use crate::native::session::{Reader, RecordReader, Store};
use agent_bridge::FirstPartyCli;
use anyhow::Context;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::super::terminal;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;

pub(super) struct AgyAdapter;

pub(super) fn result_timeout_diagnostic() -> super::super::doctor::ResultTimeoutDiagnostic {
    super::super::doctor::ResultTimeoutDiagnostic {
        probe: false,
        detail: |check, request_id| {
            (check["reason_code"] == "agy_tool_confirmation_observed"
                && check["evidence"]["request_id"] == request_id)
                .then(|| check["detail"].as_str())
                .flatten()
        },
    }
}

const PENDING_TURN_FILE: &str = "agy-pending-turn.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const FULL_TRANSCRIPT_FILE: &str = "transcript_full.jsonl";

// The only line Agy draws under its trust dialog is the footer with the selected
// model's label, and `agy models` (1.2.14) lists these families. The footer appears
// about a second after the dialog and names the saved model, not always a Gemini
// one: a `Gemini `-only rule left the dialog of a `Claude Opus 4.6 (Thinking)`
// setup unanswered until the ask deadline (session-a1QguW, 2026-10-01). Any other
// trailing line (a permission prompt, a custom status line, a new family) leaves
// the dialog to the user.
const AGY_MODEL_LABEL_PREFIXES: [&str; 3] = ["Gemini ", "Claude ", "GPT-"];

// Agy 1.2.12 has no process-local workspace-trust flag. This adapter-only
// fallback responds to its exact managed startup dialog, then verifies Agy's
// own saved decision. Remove when Agy exposes an official trust input API.
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

impl NativeProviderAdapter for AgyAdapter {
    fn workspace_trust_key(&self, screen: &str, workspace: &Path) -> Option<terminal::DialogKey> {
        agy_trust_prompt_key(screen, workspace)
    }
    fn workspace_trust(
        &self,
        workspace: &Path,
        homes: &super::super::consent::Homes,
    ) -> Result<super::super::consent::Trust> {
        use super::super::consent::{self, Evidence, Trust};
        let Some(text) = consent::read_store(&homes.agy)? else {
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
                store: homes.agy.clone(),
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
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        use super::super::doctor::{Availability::Unknown, Check};
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
            checks.push(workspace_trust_check(context.workspace, directory));
            checks.push(tool_confirmation_check(directory));
        }
        checks
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = turn::current_claim_token(&Reader::open_unchecked(context.directory))?
            .context("Agy launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let log_path = Reader::open_unchecked(context.directory)
            .private(AGY_LOG_FILE)
            .path()
            .to_owned();
        let mut arguments = vec![
            OsString::from("--log-file"),
            log_path.as_os_str().to_owned(),
        ];
        if !cfg!(windows) {
            arguments.extend([
                OsString::from("--prompt-interactive"),
                OsString::from(correlated_prompt(context.prompt, &pending)),
            ]);
        }
        Ok(LaunchPlan {
            arguments,
            prompt_is_positional: false,
            // Replace transcript polling when Agy exposes a first-party
            // per-turn completion callback with session and turn identity.
            completion_monitor: CompletionMonitor::AgyTranscript { log_path },
            environment_removals: &[],
        })
    }

    fn verify_reopen_available(&self, _provider_session_id: &str) -> Result<()> {
        bail!("{AGY_REOPEN_UNSUPPORTED}")
    }

    fn prepare_resume(&self, _context: ResumeContext<'_>) -> Result<ResumePlan> {
        bail!("{AGY_REOPEN_UNSUPPORTED}")
    }

    fn other_resumed_conversation_holders(
        &self,
        _context: ResumedSessionContext<'_>,
    ) -> Result<Vec<u32>> {
        bail!("{AGY_REOPEN_UNSUPPORTED}")
    }

    fn initial_prompt_transport(&self) -> InitialPromptTransport {
        if cfg!(windows) {
            InitialPromptTransport::TerminalPasteAfterLaunch
        } else {
            InitialPromptTransport::ProviderArgument
        }
    }

    fn initial_prompt_ready_delay(&self) -> Duration {
        // The Windows console paste waits on Agy's trust evidence and startup log
        // instead of a fixed delay (see `wait_for_startup_readiness_with`): the
        // former 12 second delay pasted onto the workspace-trust dialog of an
        // untrusted workspace, which discards the paste (issue #43). A paste that
        // still gets no receipt ends as delivery-uncertain, never as a second
        // paste. Non-Windows delivers the initial prompt as an argument and never
        // waits; its first paste is the first follow-up, which gates itself.
        Duration::ZERO
    }

    fn send_initial_prompt(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        if cfg!(windows) {
            deliver_terminal_turn(
                session,
                prompt_path,
                deadline,
                Some(WINDOWS_STARTUP_READINESS_TIMING),
            )
        } else {
            // Unreachable while `initial_prompt_transport` is `ProviderArgument` off
            // Windows; kept as the plain paste so the transport decision stays in one
            // place.
            terminal::send_file(session, prompt_path, deadline)
        }
    }

    fn terminal_initial_prompt(&self, directory: &Path, prompt: &str) -> Result<String> {
        let pending = read_pending_turn(directory)?
            .context("Agy initial turn correlation state is missing")?;
        terminal_correlated_prompt(prompt, &pending, cfg!(windows))
    }

    #[cfg(any(windows, test))]
    fn terminal_submit_count(&self) -> usize {
        1
    }

    fn follow_up_transport(&self) -> FollowUpTransport {
        // Replace this fallback when Agy exposes a verified first-party input
        // path into an already-running interactive session.
        FollowUpTransport::TerminalPasteFallback
    }

    fn new_cross_session_turn_id(&self) -> Result<String> {
        bail!("Agy does not support provider cross-session turns")
    }

    fn send_cross_session_message(
        &self,
        _context: CrossSessionMessageContext<'_>,
    ) -> CrossSessionMessageResult {
        Err(CrossSessionMessageFailure::not_sent(anyhow::anyhow!(
            "Agy does not support provider cross-session messages"
        )))
    }

    fn handle_hook(&self, _directory: &Path, _payload: &serde_json::Value) -> Result<()> {
        bail!("Agy does not use native result hooks")
    }

    fn run_control(&self, _arguments: &[String]) -> Result<()> {
        bail!("Agy does not expose Agent Bridge provider controls")
    }

    fn send_terminal_follow_up(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        if cfg!(windows) {
            // The Windows initial paste already waited for startup readiness, so a
            // follow-up only takes its receipt offset and pastes.
            deliver_terminal_turn(session, prompt_path, deadline, None)
        } else {
            // On macOS the initial prompt was a launch argument, so the first
            // follow-up is the first paste. Agy runs that argument behind its
            // workspace-trust dialog, which covers the composer until the workspace
            // is approved, and a paste onto the dialog is lost while its Enter
            // approves the folder (issue #48, see `wait_for_workspace_trust_with`).
            // Every follow-up therefore waits for the trust evidence, then for the
            // macOS readiness rule, and requires the receipt.
            deliver_terminal_turn(
                session,
                prompt_path,
                deadline,
                Some(MACOS_FOLLOW_UP_READINESS_TIMING),
            )
        }
    }

    fn prepare_terminal_follow_up(
        &self,
        directory: &Path,
        prompt: &str,
        claim_token: &str,
    ) -> Result<String> {
        let pending = install_pending_turn(directory, claim_token)?;
        terminal_correlated_prompt(prompt, &pending, cfg!(windows))
    }

    fn cancel_terminal_follow_up(&self, directory: &Path, claim_token: &str) -> Result<()> {
        cancel_pending_turn(directory, claim_token)
    }
}

fn validate_claim_token(claim_token: &str) -> Result<()> {
    if claim_token.is_empty()
        || claim_token.len() > 160
        || !claim_token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid Agy turn claim token")
    }
    Ok(())
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

// Terminal paste delivery verification (Agy adapter fallback, issues #43 and #48).
//
// Agy has no first-party input API for a running interactive session and no
// ready/accepted signal for a turn, so the adapter pastes the framed prompt into the
// managed terminal. Until Agy exposes either a first-party input path or a per-turn
// ready/accepted signal, this adapter reads Agy's own trust store and `--log-file`
// output (glog lines) as the only available evidence.
//
// What loses a paste (root cause, 2026-10-01): Agy keeps its workspace-trust dialog
// over the composer until the exact workspace is in its own trust store, and it runs
// an argument-delivered initial prompt behind the dialog. A paste onto the dialog is
// discarded and its Enter confirms the preselected "Yes, I trust this folder"; Agy
// then logs `Reloading system slash commands and skills` with three companion lines
// (356 bytes; `TrustWorkspace` -> `AddWorkspaceDir` -> `reloadWorkspaceCustomizations`
// in the 1.2.14 binary) and no `HandleUserInput`, and the session is left `working`
// at an empty composer. Reproduced on demand with Agy 1.2.14 on macOS (session-U2yPxX:
// dialog on screen, paste, those 356 bytes 0.5 s later, the workspace added to
// `trustedWorkspaces`, no receipt). In more than 100 Agy logs on that Mac
// (2026-09-10 to 2026-10-01) the reload appears only in sessions whose workspace is
// in the trust store, and in none of the sessions whose workspace never got there.
// The "deferred skills reload" of the 2026-09-24 notes below is this reload: it
// followed every lost paste by at most 1.4 s because the paste's own Enter caused
// it, and it never came in a workspace that was already trusted. It is not a startup
// timer, so no window can stand in for the trust evidence.
//
// - workspace trust: no paste before Agy's store lists the exact workspace and this
//   session's own log shows the workspace customization load, the per-process
//   evidence that its dialog is gone (`wait_for_workspace_trust_with`). The macOS
//   follow-up and the native Windows initial paste both wait for it.
// - readiness gate: `CLI startup completed` (analytics.go), on the Windows console
//   at least one `Full redraw completed` (manager.go) line after it, and a quiet
//   period in which no `Reloading system slash commands` line (with or without "and
//   skills"), no `Full redraw completed` line, and no `hooks_manager.go` line
//   arrives, measured on the gate's clock from the first read that showed the newest
//   such line. The gate is ready on the first read at which every condition holds,
//   and it never waits for a hooks completion after a reload; the startup reload of
//   session-IQHEwf had none.
//   No rule waits for the trust reload any more. The rules of 0.0.7 and 0.0.8 held
//   the paste back for a "deferred reload window" (45 s on the Windows console after
//   20 and 35 s had proved too short, 60 s on macOS) in which a `Reloading system
//   slash commands and skills` line stamped at least 1 s after `CLI startup
//   completed` ended the wait.
//   That line is the trust reload, logged when a paste's Enter approved the dialog
//   (the session-IEKjtC paste at +9.5 s, 2026-09-24 17:47, the session-fMqSQc paste
//   at +12 s, 2026-09-24 16:41, the session-ql5TVc paste at +20.1 s, 2026-09-24
//   18:48, and the session-uqraap paste at +35 s, 2026-09-24 20:44), so it never came
//   before a paste and never comes in a workspace trusted before launch, where the
//   window only delayed the paste (session-IQHEwf, 2026-09-24 17:20, logged its
//   startup reload before `CLI startup completed`, never logged another, and went
//   silent). The trust evidence is checked first instead. macOS dropped its window
//   when the cause was found. The Windows console dropped it once native Windows was
//   verified again (2026-10-01): session-jkxi48 (Agy 1.2.10, the 45 s rule) logged
//   the customization load 58 ms after startup and no trust reload before its paste
//   at +45 s, and session-C2fMs7 and session-uTpvwY (Agy 1.2.14, no window) pasted
//   right after the quiet period, 8.8 and 7.9 s after startup, and logged their
//   receipts 1.2 and 1.4 s later. `ReadinessTiming` still carries a window so that
//   the tests can replay the recorded sessions against the former rules; no rule in
//   this file sets one, and it can go together with those replays.
//   The gate also keeps the byte length and a digest of every byte of the newest
//   read (`LogContinuity`): a log that disappears, shrinks, or no longer reproduces
//   that digest over the observed length was replaced or rotated, so every
//   settlement instant is discarded and the quiet period is re-measured from the new
//   content. Any number of such restarts is tolerated within the deadline: the gate
//   fails the paste as `not_sent` only when the deadline passes, and the report then
//   lists every discontinuity in order (review round 6, replacing round 5's
//   leading-bytes check and its failure on the second discontinuity).
// - input receipt: a complete `HandleUserInput called with text: "..."` line
//   (input_loop.go) that starts after the byte length of agy.log observed
//   immediately before the paste and whose text carries the adapter's framing and
//   the complete pending turn marker.
//
// Delivery classification after a paste (issue #43 review):
//
// - delivered: such a receipt line exists after the pre-paste offset. Nothing
//   logged after the receipt revokes it. Agy 1.2.10 logs `Reloading system slash
//   commands and skills` a few milliseconds after `HandleUserInput` and the
//   conversation-start lines (`Starting new conversation`, `Created conversation`):
//   session-udT6uY logged it 15 ms after the receipt (2026-09-24 16:42:50.228), and
//   both delivered live asks of round 7 showed the same. That is the reload the new
//   conversation triggers, not the trust reload, so it is never evidence that the
//   paste was lost. `observe_input_receipt` looks only for the receipt, and
//   `confirm_input_receipt_with` returns on the first read that yields `Delivered`,
//   so a later reload cannot flip a delivered classification;
// - delivery-uncertain: everything else. Non-delivery would have to be proven by a
//   line Agy logs after draining its console input without a receipt, and the real
//   logs contain no such marker (session-fMqSQc, 2026-09-24: after the discarded
//   paste Agy logged only the trust reload and then nothing for a minute), so a
//   missing receipt at the end of the window, a deadline-capped window, an
//   unreadable or missing log, a log shorter than the pre-paste offset (rotated or
//   truncated), and a partial trailing line all stay uncertain and never `not_sent`.
//   The paste is never repeated. A lost paste therefore ends as delivery-uncertain:
//   the launcher keeps the turn claim, leaves the session `working`, and writes the
//   receipt error to `status.error` (`Claim::settle_delivery` for both
//   the initial prompt and `tell`).
//   The caller must inspect that error (or `doctor <session>`) and then close the
//   session with `close-session --explicit` or launch a new one; the bridge never
//   re-pastes or cleans up on its own.
//
// macOS follow-up delivery (iTerm2/Terminal.app paste): the initial prompt is a
// `--prompt-interactive` argument, so the first paste is the first `tell`. Three
// differences from the Windows console:
//
// - The trust evidence is checked at the follow-up, not at launch: the argument
//   prompt completes behind the dialog, so an `ask` in a workspace no provider has
//   approved returns its result while the dialog is still up. Issue #48 was that
//   state: the former 60 s window ended, the paste landed on the dialog, and the
//   receipt check saw only the 356 bytes of the trust reload.
// - There is no deferred-reload window (`MACOS_FOLLOW_UP_READINESS_TIMING`): once the
//   trust evidence holds nothing is pending, and a follow-up in a workspace trusted
//   before launch no longer waits 60 s for a reload that never comes.
// - Agy on macOS logs no `Full redraw completed` line at all (session-QMFk6F and
//   session-7IgCnx, this machine), so the macOS rule does not require the redraw
//   marker. The argument prompt starts a conversation a few seconds after startup,
//   and the `Reloading system slash commands and skills` line a few milliseconds
//   after `Starting new conversation` (7 ms in session-QMFk6F at +2.9 s, 26 ms in
//   session-7IgCnx at +2.7 s) is the conversation reload: an activity line for the
//   quiet period, and never the trust reload on either platform
//   (`CONVERSATION_RELOAD_MAX_LATENCY`).
//
// The receipt is the same Go-quoted `HandleUserInput` line: a multi-line paste
// through iTerm2 is logged as one record with `\n` escapes and the complete marker
// (session-QMFk6F, 21:42:19). It carries the macOS framing header instead of the
// Windows one, and `receipt_matches` accepts either adapter-owned framing.
//
// Delete this section, `initial_prompt_ready_delay`, and the trust and receipt
// branches of `send_initial_prompt`/`send_terminal_follow_up` when Agy provides
// such a signal or an input API; the transcript result monitor is unaffected.
const AGY_LOG_FILE: &str = "agy.log";
const STARTUP_COMPLETED_MARKER: &str = "CLI startup completed";
const SLASH_RELOAD_MARKER: &str = "Reloading system slash commands";
const SKILLS_RELOAD_MARKER: &str = "Reloading system slash commands and skills";
const HOOKS_LOADED_SOURCE: &str = "hooks_manager.go";
const FULL_REDRAW_MARKER: &str = "Full redraw completed";
const REDRAW_AFTER_STARTUP_DESCRIPTION: &str =
    "`Full redraw completed` after `CLI startup completed`";
const DEFERRED_RELOAD_DESCRIPTION: &str = "deferred skills reload or the deferred reload window (no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and outside 1 s after a `Starting new conversation` line, and the window since `CLI startup completed` was observed has not elapsed)";
const QUIET_PERIOD_DESCRIPTION: &str = "quiet period not reached (no `Reloading system slash commands`, `Full redraw completed`, or `hooks_manager.go` line for the quiet period after the newest one)";
const CONVERSATION_START_MARKER: &str = "Starting new conversation";
const INPUT_RECEIPT_MARKER: &str = "HandleUserInput called with text: \"";
const WINDOWS_PROTOCOL_PREFIX: &str = "[Agent Bridge Agy Windows console turn protocol]";
// The framing header of `correlated_prompt`, which macOS pastes verbatim.
const TURN_PROTOCOL_HEADER: &str = "[Agent Bridge Agy turn protocol]";
// A skills reload stamped at most this long after the newest `Starting new
// conversation` line is the reload the new conversation triggers (7 ms and 26 ms
// after it on macOS; 15 ms after the receipt on Windows, session-udT6uY), never the
// trust reload, which follows the Enter that approves the workspace-trust dialog.
const CONVERSATION_RELOAD_MAX_LATENCY: Duration = Duration::from_secs(1);
// Observed post-login reload bursts arrive about 3.0 seconds apart; the quiet period
// must outlast that cadence so the paste does not land between two of them.
const STARTUP_QUIET_PERIOD: Duration = Duration::from_millis(3500);
// Trust reload latency after `CLI startup completed`, Agy 1.2.10 on the Windows
// machine, 2026-09-24 KST (the fixtures in the tests):
//
// | session        | latency | window at the time | paste                       |
// |----------------|---------|--------------------|-----------------------------|
// | session-IEKjtC |  9.8 s  | none (fixed 12 s)  | +9.5 s, lost                |
// | session-fMqSQc | 13.0 s  | none (fixed 12 s)  | +12 s, lost                 |
// | session-ql5TVc | 21.4 s  | 20 s               | +20.1 s, lost               |
// | session-Cf1FBY | 36.4 s  | 35 s               | +35 s, lost (heavy CPU load)|
// | session-uqraap | 36.4 s  | 35 s               | +35 s, lost (CPU idle)      |
// | session-IQHEwf | never   | -                  | none (round-1 gate)         |
//
// Nine more logs put it at 12.9 to 13.2 s, about a second after the fixed 12 s
// paste. The reload always trails the paste, whatever the window was: the paste's
// Enter approved the workspace-trust dialog and Agy logged the trust reload, so each
// longer window only moved the loss later, and session-IQHEwf, which pasted nothing,
// never logged it. No window was ever a measured startup latency, and none is left
// (see the note at the head of this section).
//
// A `Reloading system slash commands and skills` line stamped less than this long
// after `CLI startup completed` is the startup reload, not the trust reload. Agy
// logs its startup reload on either side of `CLI startup completed`: 3.5 ms before
// it in session-fMqSQc, 0.5 ms after it in session-udT6uY and 1.6 ms after it in
// session-M8QFPp (2026-09-24 20:37), where the former rule took it for the later
// reload and settled the condition at once. The trust reload follows an Enter on
// the dialog; the earliest one recorded on the Windows console came 9.8 s after
// startup.
const DEFERRED_RELOAD_MIN_LATENCY: Duration = Duration::from_secs(1);
// The Windows console rule: Agy redraws the console composer once the TUI is up, so
// the redraw after startup is required. The initial paste has already waited for the
// trust evidence, so no trust reload is pending and there is no deferred-reload
// window: the former 45 s one delayed every `ask` in a workspace trusted before
// launch by that long (53 s for session-jkxi48 against 18 and 24 s without it,
// 2026-10-01).
const WINDOWS_STARTUP_READINESS_TIMING: ReadinessTiming = ReadinessTiming {
    quiet_period: STARTUP_QUIET_PERIOD,
    deferred_reload_window: Duration::ZERO,
    redraw_required: true,
};
// The macOS follow-up rule: the same quiet period, no deferred-reload window, and no
// redraw marker, which Agy never logs on macOS. The follow-up has already waited for
// the trust evidence, so no trust reload is pending. The former 60 s window
// (2026-09-24: 11.2 s in session-QMFk6F, 55.4 s in session-S7qq65, both the trust
// reload that the `tell`'s own Enter caused) delayed every first follow-up in a
// trusted workspace by a minute and still pasted onto the dialog of an untrusted one
// (issue #48).
const MACOS_FOLLOW_UP_READINESS_TIMING: ReadinessTiming = ReadinessTiming {
    quiet_period: STARTUP_QUIET_PERIOD,
    deferred_reload_window: Duration::ZERO,
    redraw_required: false,
};
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
// Agy logs `HandleUserInput` when it processes the pasted line, not when the console
// receives it, and that processing is slow under load: session-M8QFPp (2026-09-24
// 20:37) logged the receipt 18.0 s after the paste, outside the former 15 s window,
// so the adapter reported delivery-uncertain although the paste had landed;
// session-8WjG3m logged it about 10 s after its paste. The receipt is still capped
// by the deadline, and a lost paste still ends as delivery-uncertain, only 45 s
// later than before.
const INPUT_RECEIPT_WINDOW: Duration = Duration::from_secs(60);
const INPUT_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

trait Clock {
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

fn read_log_bytes(log_path: &Path) -> Result<Option<Vec<u8>>> {
    RecordReader::at(log_path).bytes()
}

// The trust evidence a paste needs (issue #48): Agy's trust store lists the exact
// workspace, and this session's own log shows that this Agy process loaded the
// workspace customizations. The store alone is shared by every Agy process: a
// dialog approved in one session leaves the dialog of another session of the same
// workspace open (session-UuYk87, 2026-10-01: store entry present, dialog still on
// screen for five seconds until it was approved there too). Agy loads the
// customizations only once it trusts the workspace, at startup when the store
// already lists it or when the dialog is approved in that process (`AddWorkspaceDir`
// -> `reloadWorkspaceCustomizations` -> `ReloadHooks`), and either way a goroutine
// other than the main one logs a `hooks_manager.go` line; the main goroutine logs
// the only other one while the store manager is built. In 112 logs of Agy 1.2.6 to
// 1.2.14 on macOS and in the Windows fixtures of the tests, every delivered paste
// follows such a line and no session with its dialog still open has one.
//
// A log without the line withholds the paste but does not prove an open dialog: the
// log can be missing, or cut before the line, in a session that has none. The
// reports therefore state what was read, and the recovery is conditional on what the
// managed terminal shows.
const TRUST_STORE_MISSING: &str = "Agy's trust store does not list the exact workspace, and Agy keeps its trust dialog over the composer until such a workspace is approved";
const TRUST_SESSION_UNVERIFIED: &str = "Agy's trust store lists the workspace, but this session's agy.log does not show that this session loaded the workspace customizations (no `hooks_manager.go` line outside the main goroutine), so its own trust dialog may still be open";
const TRUST_RECOVERY: &str = "If the managed terminal shows the trust dialog, approve it there and send the prompt again; if it shows the composer or the prompt is still withheld afterwards, close this session and start a new one";

fn workspace_customizations_loaded(log: &[u8]) -> bool {
    complete_log_lines(log).any(|line| {
        glog_timestamp(&line).is_some()
            && line
                .get(21..)
                .and_then(|rest| rest.trim_start().split_once(' '))
                .is_some_and(|(goroutine, rest)| {
                    goroutine != "1"
                        && !goroutine.is_empty()
                        && goroutine.bytes().all(|byte| byte.is_ascii_digit())
                        && rest.starts_with(HOOKS_LOADED_SOURCE)
                })
    })
}

// Waits until `missing` reports nothing missing. An absent entry, a session whose
// log lacks the load, an unreadable store or log, and a lookup error all keep
// waiting, since none proves the dialog is gone: a human approves it in the managed
// terminal, or shared consent answered it at launch. The deadline ends the wait as
// `not_sent`, before any terminal input, and so does a session that has ended
// (`ended` names its state): it has no terminal left to paste into, and the launcher
// of a closed session went on waiting until its deadline (issue #60).
fn wait_for_workspace_trust_with<T, C>(
    missing: &mut T,
    ended: &mut dyn FnMut() -> Option<String>,
    deadline: Instant,
    poll_interval: Duration,
    clock: &mut C,
) -> Result<()>
where
    T: FnMut() -> Result<Option<&'static str>>,
    C: Clock,
{
    loop {
        if let Some(state) = ended() {
            bail!(
                "the session is {state} and no longer waits for Agy workspace trust, so the prompt was not pasted"
            );
        }
        let observed = missing();
        if matches!(observed, Ok(None)) {
            return Ok(());
        }
        let now = clock.now();
        if now >= deadline {
            let reason = match observed {
                Err(error) => format!("Agy's trust evidence could not be read ({error:#})"),
                Ok(missing) => missing.unwrap_or_default().to_owned(),
            };
            bail!(
                "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {reason}. {TRUST_RECOVERY}"
            );
        }
        clock.sleep(deadline.saturating_duration_since(now).min(poll_interval));
    }
}

fn workspace_trust_missing(
    workspace: &Path,
    homes: &super::super::consent::Homes,
    log_path: &Path,
) -> Result<Option<&'static str>> {
    use super::super::consent::Trust;
    if !matches!(
        ADAPTER.workspace_trust(workspace, homes)?,
        Trust::Trusted(_)
    ) {
        return Ok(Some(TRUST_STORE_MISSING));
    }
    let loaded = read_log_bytes(log_path)?
        .as_deref()
        .is_some_and(workspace_customizations_loaded);
    Ok((!loaded).then_some(TRUST_SESSION_UNVERIFIED))
}

fn wait_for_workspace_trust(directory: &Path, deadline: Instant) -> Result<()> {
    let workspace = Reader::open_unchecked(directory).manifest()?.workspace;
    let homes = super::super::consent::Homes::current()?;
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    wait_for_workspace_trust_with(
        &mut || workspace_trust_missing(&workspace, &homes, &log_path),
        &mut || {
            Reader::open_unchecked(directory)
                .status()
                .ok()
                .map(|status| status.state)
                .filter(|state| {
                    matches!(
                        state,
                        SessionState::Failed | SessionState::Exited | SessionState::Closed
                    )
                })
                .map(|state| state.to_string())
        },
        deadline,
        STARTUP_POLL_INTERVAL,
        &mut SystemClock,
    )
}

// Only preparation can produce a paste-ready turn. Consuming it issues one paste
// and checks the receipt against the offset from the read that passed readiness.
struct PreparedTerminalTurn<'a> {
    directory: &'a Path,
    pending: PendingAgyTurn,
    pre_paste_len: usize,
}

impl<'a> PreparedTerminalTurn<'a> {
    fn prepare_with(
        directory: &'a Path,
        trust: impl FnOnce() -> Result<()>,
        readiness: impl FnOnce() -> Result<usize>,
    ) -> Result<Self> {
        trust()?;
        let pending =
            read_pending_turn(directory)?.context("Agy turn correlation state is missing")?;
        let pre_paste_len = readiness()?;
        Ok(Self {
            directory,
            pending,
            pre_paste_len,
        })
    }

    fn deliver(
        self,
        terminal: &str,
        send: impl FnOnce() -> terminal::TerminalSendResult,
        confirm: impl FnOnce(&PendingAgyTurn, usize) -> terminal::TerminalSendResult,
    ) -> terminal::TerminalSendResult {
        trace_terminal_delivery(
            self.directory,
            &self.pending,
            self.pre_paste_len,
            terminal,
            send,
            || confirm(&self.pending, self.pre_paste_len),
        )
    }
}

// A Windows follow-up has already passed startup trust/readiness in its initial
// paste; all other pastes wait for both gates here before preparing the turn.
fn deliver_terminal_turn(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    readiness: Option<ReadinessTiming>,
) -> terminal::TerminalSendResult {
    use terminal::TerminalSendFailure;
    let directory = session_directory_of(session).map_err(TerminalSendFailure::not_sent)?;
    let log_path = Reader::open_unchecked(&directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let prepared = PreparedTerminalTurn::prepare_with(
        &directory,
        || {
            if readiness.is_some() {
                wait_for_workspace_trust(&directory, deadline)?;
            }
            Ok(())
        },
        || {
            if let Some(timing) = readiness {
                // No later log read may replace the gate's receipt offset.
                wait_for_startup_readiness_with(
                    &mut || read_log_bytes(&log_path),
                    deadline,
                    timing,
                    STARTUP_POLL_INTERVAL,
                    &mut SystemClock,
                )
            } else {
                let log = read_log_bytes(&log_path)
                    .context("Agy log could not be read before the console paste")?;
                follow_up_pre_paste_offset(log.as_deref())
            }
        },
    )
    .map_err(TerminalSendFailure::not_sent)?;
    prepared.deliver(
        session.kind.as_str(),
        || terminal::send_file(session, prompt_path, deadline),
        |pending, pre_paste_len| {
            // The composer after an unconfirmed paste is unknown; never paste again.
            confirm_input_receipt_with(
                &mut || read_log_bytes(&log_path),
                pending,
                pre_paste_len,
                Instant::now(),
                deadline,
                INPUT_RECEIPT_POLL_INTERVAL,
                &mut SystemClock,
            )
        },
    )
}

// Request-scoped timing evidence for #48. A missing receipt alone cannot locate
// the failure between terminal injection, a late reload, and Agy's input loop.
// Record actual dispatch times and the offset used by the receipt check without
// copying prompt text. Diagnostic write failures never cancel or retry delivery.
fn trace_terminal_delivery<S, C>(
    directory: &Path,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    terminal: &str,
    send: S,
    confirm: C,
) -> terminal::TerminalSendResult
where
    S: FnOnce() -> terminal::TerminalSendResult,
    C: FnOnce() -> terminal::TerminalSendResult,
{
    let record = Store::open_unchecked(directory)
        .private(&format!("agy-input-{}.json", pending.claim_token));
    let mut trace = serde_json::json!({
        "schema":1, "claim_token":pending.claim_token, "terminal":terminal,
        "pre_paste_offset":pre_paste_len, "prepared_unix_ms":super::super::unix_ms(),
        "outcome":"prepared", "paste_started_unix_ms":null,
        "paste_returned_unix_ms":null, "receipt_finished_unix_ms":null, "error":null,
    });
    let _ = record.write_json(&trace);
    trace["paste_started_unix_ms"] = serde_json::json!(super::super::unix_ms());
    let result = send();
    trace["paste_returned_unix_ms"] = serde_json::json!(super::super::unix_ms());
    let result = result.and_then(|()| confirm());
    trace["receipt_finished_unix_ms"] = serde_json::json!(super::super::unix_ms());
    trace["outcome"] = serde_json::json!(match &result {
        Ok(()) => "confirmed",
        Err(error) if error.delivery_may_have_occurred() => "delivery-uncertain",
        Err(_) => "not-sent",
    });
    if let Err(error) = &result {
        trace["error"] = serde_json::json!(error.error().to_string());
    }
    let _ = record.write_json(&trace);
    result
}

// A follow-up is pasted into a session whose Agy has been logging since launch, so
// the log it will write the receipt to already exists. A missing log is a
// discontinuity, never offset zero: pasting against it could only end uncertain.
fn follow_up_pre_paste_offset(log: Option<&[u8]>) -> Result<usize> {
    log.map(<[u8]>::len).context(
        "agy.log is missing before the console paste, so no input receipt could be observed; the follow-up was not pasted",
    )
}

fn session_directory_of(session: &terminal::TerminalSession) -> Result<PathBuf> {
    let id = session
        .managed_session_id
        .as_deref()
        .context("Agy terminal handle is missing its managed session binding")?;
    Reader::session_directory(id)
}

fn input_receipt_window_end(now: Instant, deadline: Instant) -> Instant {
    now.checked_add(INPUT_RECEIPT_WINDOW)
        .map_or(deadline, |end| end.min(deadline))
}

// Complete log lines only: a line without its terminating newline may still be
// written, so it is neither a marker nor a receipt yet. Terminal escape sequences and
// carriage returns are dropped so a marker is recognised through console noise.
fn complete_log_lines(log: &[u8]) -> impl Iterator<Item = String> + '_ {
    let complete = match log.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &log[..end],
        None => &log[..0],
    };
    complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| strip_terminal_noise(&String::from_utf8_lossy(line)))
}

fn strip_terminal_noise(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut clean = String::with_capacity(line.len());
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '\u{1b}' => {
                index += 1;
                match characters.get(index) {
                    Some('[') => {
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            index += 1;
                            if ('\u{40}'..='\u{7e}').contains(next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC ends with BEL or with the string terminator `ESC \`.
                        // Any other escape ends the OSC and is processed on its own.
                        index += 1;
                        while let Some(next) = characters.get(index) {
                            if *next == '\u{7}' {
                                index += 1;
                                break;
                            }
                            if *next == '\u{1b}' {
                                if characters.get(index + 1) == Some(&'\\') {
                                    index += 2;
                                }
                                break;
                            }
                            index += 1;
                        }
                    }
                    Some(_) => index += 1,
                    None => {}
                }
            }
            '\r' => index += 1,
            other => {
                clean.push(other);
                index += 1;
            }
        }
    }
    clean
}

// The glog `[IWEF]MMDD HH:MM:SS.uuuuuu` stamp of a line as an offset from an
// arbitrary epoch (months counted as 31 days); only the difference between two
// stamps of the same log is meaningful, and a difference across a year boundary is
// not. Agy stamps in local time. A line without a stamp (`CLI ready for user
// input`, a file header, console noise) yields `None`.
fn glog_timestamp(line: &str) -> Option<Duration> {
    let bytes = line.as_bytes();
    if !matches!(bytes.first(), Some(b'I' | b'W' | b'E' | b'F')) || bytes.get(5) != Some(&b' ') {
        return None;
    }
    let date = line.get(1..5)?;
    let month: u64 = date.get(..2)?.parse().ok()?;
    let day: u64 = date.get(2..)?.parse().ok()?;
    let stamp = line.get(6..21)?;
    let (hours, rest) = stamp.split_once(':')?;
    let (minutes, seconds) = rest.split_once(':')?;
    let (seconds, micros) = seconds.split_once('.')?;
    if micros.len() != 6 {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: u64 = seconds.parse().ok()?;
    let micros: u64 = micros.parse().ok()?;
    Some(
        Duration::from_secs((month * 31 + day) * 86_400 + hours * 3600 + minutes * 60 + seconds)
            + Duration::from_micros(micros),
    )
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StartupObservation {
    // Line index of the first `CLI startup completed` line.
    startup_line: Option<usize>,
    // The glog stamp of that line; `None` when it carries none, in which case no
    // skills reload can be proven deferred and only the window settles the condition.
    startup_stamp: Option<Duration>,
    // Line index of the first `Full redraw completed` line after the startup line.
    // Agy redraws the composer once the TUI is up; the redraw before startup, when
    // there is one, does not count.
    redraw_after_startup: Option<usize>,
    // Line index of the first `Reloading system slash commands and skills` line
    // after the startup line that is stamped at least `DEFERRED_RELOAD_MIN_LATENCY`
    // after it: the trust reload Agy logs when its workspace-trust dialog is
    // approved (the "deferred" reload of the 2026-09-24 notes). The startup
    // reload, which Agy logs a few milliseconds before or after `CLI startup
    // completed`, does not count on either side of it.
    deferred_reload_after_startup: Option<usize>,
    // Line index of the first skills reload after the startup line that was too
    // early to be the deferred reload (diagnostics only).
    startup_reload_after_startup: Option<usize>,
    // Line index of the first skills reload after the startup line that followed a
    // `Starting new conversation` line within `CONVERSATION_RELOAD_MAX_LATENCY`: the
    // reload a new conversation triggers, not the deferred one (diagnostics only).
    conversation_reload_after_startup: Option<usize>,
    // The glog stamp of the newest `Starting new conversation` line so far.
    conversation_start_stamp: Option<Duration>,
    // Line index of the newest activity line: any `Reloading system slash commands`
    // line (with or without "and skills"), `Full redraw completed` line, or
    // `hooks_manager.go` line, wherever it sits. The quiet period restarts whenever
    // this changes.
    settle_line: Option<usize>,
    // Activity lines seen so far (diagnostics only).
    activity_lines: usize,
    // The newest glog stamp in the log: how long Agy itself says it has been running
    // since `CLI startup completed`. A follow-up gated long after startup settles the
    // deferred-reload window from this, not from the gate's own clock, so it does not
    // wait a whole window for a reload that would already have been logged.
    newest_stamp: Option<Duration>,
}

impl StartupObservation {
    fn startup_completed(&self) -> bool {
        self.startup_line.is_some()
    }

    // The startup markers the platform rule requires: startup everywhere, plus the
    // redraw where Agy logs one (the Windows console).
    fn markers_observed(&self, redraw_required: bool) -> bool {
        self.startup_completed() && (!redraw_required || self.redraw_after_startup.is_some())
    }

    fn missing_markers(&self, redraw_required: bool) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.startup_line.is_none() {
            missing.push("`CLI startup completed`");
        }
        if redraw_required && self.redraw_after_startup.is_none() {
            missing.push(REDRAW_AFTER_STARTUP_DESCRIPTION);
        }
        missing
    }
}

fn is_activity_line(line: &str) -> bool {
    line.contains(SLASH_RELOAD_MARKER)
        || line.contains(FULL_REDRAW_MARKER)
        || line.contains(HOOKS_LOADED_SOURCE)
}

fn observe_startup(log: &[u8]) -> StartupObservation {
    let mut observation = StartupObservation::default();
    for (index, line) in complete_log_lines(log).enumerate() {
        if let Some(stamp) = glog_timestamp(&line)
            && observation.newest_stamp.is_none_or(|newest| stamp > newest)
        {
            observation.newest_stamp = Some(stamp);
        }
        if observation.startup_line.is_none() && line.contains(STARTUP_COMPLETED_MARKER) {
            observation.startup_line = Some(index);
            observation.startup_stamp = glog_timestamp(&line);
        }
        if observation.startup_line.is_some()
            && observation.redraw_after_startup.is_none()
            && line.contains(FULL_REDRAW_MARKER)
        {
            observation.redraw_after_startup = Some(index);
        }
        if line.contains(CONVERSATION_START_MARKER)
            && let Some(stamp) = glog_timestamp(&line)
        {
            observation.conversation_start_stamp = Some(stamp);
        }
        if observation
            .startup_line
            .is_some_and(|startup| startup < index)
            && observation.deferred_reload_after_startup.is_none()
            && line.contains(SKILLS_RELOAD_MARKER)
        {
            // Threads stamp out of order by a few milliseconds, so the reload's
            // stamp can precede the startup stamp; a stamp that cannot be read
            // proves nothing and the reload is taken for the startup one.
            let reload_stamp = glog_timestamp(&line);
            let deferred = match (observation.startup_stamp, reload_stamp) {
                (Some(startup), Some(reload)) => {
                    reload.saturating_sub(startup) >= DEFERRED_RELOAD_MIN_LATENCY
                }
                _ => false,
            };
            // A reload right after a conversation start is that conversation's
            // reload: the argument-delivered macOS initial prompt starts one a few
            // seconds after startup, before the deferred reload.
            let conversation_reload = match (observation.conversation_start_stamp, reload_stamp) {
                (Some(started), Some(reload)) => {
                    reload >= started
                        && reload.saturating_sub(started) <= CONVERSATION_RELOAD_MAX_LATENCY
                }
                _ => false,
            };
            if conversation_reload {
                if observation.conversation_reload_after_startup.is_none() {
                    observation.conversation_reload_after_startup = Some(index);
                }
            } else if deferred {
                observation.deferred_reload_after_startup = Some(index);
            } else if observation.startup_reload_after_startup.is_none() {
                observation.startup_reload_after_startup = Some(index);
            }
        }
        if is_activity_line(&line) {
            observation.activity_lines += 1;
            observation.settle_line = Some(index);
        }
    }
    observation
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadinessState {
    Ready,
    AwaitingLog,
    AwaitingStartup,
    AwaitingRedraw,
    AwaitingDeferredReload,
    Settling,
}

impl ReadinessState {
    fn describe(self) -> &'static str {
        match self {
            Self::Ready => "startup readiness observed",
            Self::AwaitingLog => "agy.log has not been created",
            Self::AwaitingStartup => "agy.log has no `CLI startup completed` line",
            Self::AwaitingRedraw => {
                "agy.log has no `Full redraw completed` line after `CLI startup completed`"
            }
            Self::AwaitingDeferredReload => {
                "agy.log has no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and the deferred reload window since `CLI startup completed` has not elapsed"
            }
            Self::Settling => {
                "agy.log logged a reload, redraw, or hooks line within the quiet period"
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ReadinessTiming {
    quiet_period: Duration,
    // How long the former rules waited for the trust reload after startup. Zero in
    // every rule of this file; the tests set it to replay those rules.
    deferred_reload_window: Duration,
    // Whether a `Full redraw completed` line after startup is required. Agy logs it
    // on the Windows console and never on macOS.
    redraw_required: bool,
}

// The settlement instants are only evidence about the log they were measured
// against. Agy appends to its `--log-file`, so every read must extend the previous
// one; a log that disappears, shrinks, or no longer holds every byte observed
// earlier was replaced or rotated, and the indices of its `CLI startup completed`
// and activity lines can coincide with the old ones while the composer behind it
// is fresh. The gate therefore keeps the byte length and a digest of every byte of
// the newest read, the way the receipt check keeps the pre-paste offset, and on a
// discontinuity discards every settlement instant and starts over from the new
// content: the quiet period and the deferred reload window are timed from the new
// observation. Any number of discontinuities is tolerated within the deadline; each
// one restarts the evidence, and the deadline report counts them. A read is a
// continuation only when it is at least as long as the observed content and its
// first `len` bytes digest to the same value, so a rewrite anywhere inside the
// observed content (a re-stamped prefix, a replaced later line, or a suffix rewrite
// that keeps every line index) is a discontinuity, not just one within a leading
// window. The log is small, so the prefix is re-digested on every read.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LogContinuity {
    len: usize,
    digest: u64,
}

fn log_digest(bytes: &[u8]) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

impl LogContinuity {
    fn of(log: &[u8]) -> Self {
        Self {
            len: log.len(),
            digest: log_digest(log),
        }
    }

    // Whether `log` continues the observed content, or how it breaks from it.
    fn discontinuity(&self, log: &[u8]) -> Option<LogDiscontinuity> {
        if log.len() < self.len {
            Some(LogDiscontinuity::Shrunk {
                from: self.len,
                to: log.len(),
            })
        } else if log_digest(&log[..self.len]) != self.digest {
            Some(LogDiscontinuity::Replaced { observed: self.len })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogDiscontinuity {
    // The log disappeared after content had been observed.
    Missing { observed: usize },
    // The log is shorter than the observed content (rotated or truncated).
    Shrunk { from: usize, to: usize },
    // The log no longer begins with every byte observed earlier (replaced).
    Replaced { observed: usize },
}

impl LogDiscontinuity {
    fn describe(self) -> String {
        match self {
            Self::Missing { observed } => {
                format!("agy.log disappeared after {observed} bytes had been observed")
            }
            Self::Shrunk { from, to } => {
                format!("agy.log shrank from {from} to {to} bytes (rotated or truncated)")
            }
            Self::Replaced { observed } => format!(
                "agy.log no longer begins with the bytes observed earlier ({observed} bytes had been observed; replaced)"
            ),
        }
    }
}

struct ReadinessGate {
    timing: ReadinessTiming,
    started_at: Instant,
    settle_line: Option<usize>,
    settled_at: Instant,
    quiet_reached: bool,
    // When the gate first saw `CLI startup completed`; the deferred reload window
    // counts from here, on the gate's clock, like the quiet period.
    startup_seen_at: Option<Instant>,
    deferred_reload_settled: bool,
    last_observation: Option<StartupObservation>,
    // The newest observed log content; `None` until a log has been read and after
    // it disappears.
    continuity: Option<LogContinuity>,
    // Every discontinuity so far, with the time since the gate started.
    discontinuities: Vec<(Duration, LogDiscontinuity)>,
}

impl ReadinessGate {
    fn new(now: Instant, timing: ReadinessTiming) -> Self {
        Self {
            timing,
            started_at: now,
            settle_line: None,
            settled_at: now,
            quiet_reached: false,
            startup_seen_at: None,
            deferred_reload_settled: false,
            last_observation: None,
            continuity: None,
            discontinuities: Vec::new(),
        }
    }

    // Discards every settlement instant: the next observation starts over as if the
    // gate had just been created, with the quiet period and the deferred reload
    // window timed from `now`.
    fn restart_after(&mut self, discontinuity: LogDiscontinuity, now: Instant) {
        self.discontinuities.push((
            now.saturating_duration_since(self.started_at),
            discontinuity,
        ));
        self.settle_line = None;
        self.settled_at = now;
        self.quiet_reached = false;
        self.startup_seen_at = None;
        self.deferred_reload_settled = false;
        self.last_observation = None;
        self.continuity = None;
    }

    // Byte length of the newest observed log; the receipt baseline when that
    // observation passed the gate.
    fn observed_len(&self) -> Option<usize> {
        self.continuity.as_ref().map(|continuity| continuity.len)
    }

    fn observe(&mut self, log: Option<&[u8]>, now: Instant) -> ReadinessState {
        let Some(log) = log else {
            if let Some(previous) = self.continuity.take() {
                self.restart_after(
                    LogDiscontinuity::Missing {
                        observed: previous.len,
                    },
                    now,
                );
            }
            return ReadinessState::AwaitingLog;
        };
        if let Some(discontinuity) = self
            .continuity
            .as_ref()
            .and_then(|previous| previous.discontinuity(log))
        {
            self.restart_after(discontinuity, now);
        }
        self.continuity = Some(LogContinuity::of(log));
        let observation = observe_startup(log);
        if observation.settle_line != self.settle_line {
            self.settle_line = observation.settle_line;
            self.settled_at = now;
        }
        self.quiet_reached =
            now.saturating_duration_since(self.settled_at) >= self.timing.quiet_period;
        if observation.startup_completed() && self.startup_seen_at.is_none() {
            self.startup_seen_at = Some(now);
        }
        // The window is measured on the gate's clock from the first read that showed
        // startup, and also on Agy's own clock from the startup stamp to the newest
        // stamp: a session that has already logged a window's worth of runtime cannot
        // still be inside its startup burst (session-bbNK3d, macOS, 2026-09-24 22:03,
        // never logged the deferred reload and waited the whole window on each of two
        // follow-ups minutes after startup).
        let logged_window_elapsed = match (observation.startup_stamp, observation.newest_stamp) {
            (Some(startup), Some(newest)) => {
                newest.saturating_sub(startup) >= self.timing.deferred_reload_window
            }
            _ => false,
        };
        self.deferred_reload_settled = observation.deferred_reload_after_startup.is_some()
            || logged_window_elapsed
            || self.startup_seen_at.is_some_and(|seen| {
                now.saturating_duration_since(seen) >= self.timing.deferred_reload_window
            });
        let state = if !observation.startup_completed() {
            ReadinessState::AwaitingStartup
        } else if self.timing.redraw_required && observation.redraw_after_startup.is_none() {
            ReadinessState::AwaitingRedraw
        } else if !self.deferred_reload_settled {
            ReadinessState::AwaitingDeferredReload
        } else if !self.quiet_reached {
            ReadinessState::Settling
        } else {
            ReadinessState::Ready
        };
        self.last_observation = Some(observation);
        state
    }

    fn deadline_report(&self, state: ReadinessState) -> String {
        let mut missing = match &self.last_observation {
            Some(observation) => observation.missing_markers(self.timing.redraw_required),
            None => StartupObservation::default().missing_markers(self.timing.redraw_required),
        };
        // No rule of this file has a window; the report names one only for a rule
        // that does.
        let windowed = !self.timing.deferred_reload_window.is_zero();
        if windowed && !self.deferred_reload_settled {
            missing.push(DEFERRED_RELOAD_DESCRIPTION);
        }
        if !self.quiet_reached {
            missing.push(QUIET_PERIOD_DESCRIPTION);
        }
        let missing = if missing.is_empty() {
            "none".to_owned()
        } else {
            missing.join(", ")
        };
        let restarted = match self.discontinuities.len() {
            0 => String::new(),
            1 => format!(
                "; the readiness evidence was restarted after 1 log discontinuity ({})",
                self.describe_discontinuities()
            ),
            count => format!(
                "; the readiness evidence was restarted after {count} log discontinuities ({})",
                self.describe_discontinuities()
            ),
        };
        let timed = if windowed {
            format!(
                "the quiet period is {} ms and the deferred reload window is {} ms, timed concurrently (the window from the first read with `CLI startup completed`, the quiet period from the newest reload, redraw, or hooks line; ready when both hold)",
                self.timing.quiet_period.as_millis(),
                self.timing.deferred_reload_window.as_millis()
            )
        } else {
            format!(
                "the quiet period is {} ms, timed from the newest reload, redraw, or hooks line",
                self.timing.quiet_period.as_millis()
            )
        };
        format!(
            "Agy did not report startup readiness before the deadline: {}; missing markers: {missing}; {timed}{restarted}; the prompt was not pasted",
            state.describe(),
        )
    }

    fn describe_discontinuities(&self) -> String {
        self.discontinuities
            .iter()
            .map(|(at, discontinuity)| {
                format!("{} at {} ms", discontinuity.describe(), at.as_millis())
            })
            .collect::<Vec<_>>()
            .join("; then ")
    }
}

// Waits until one read of the log passes the readiness rule and continues every
// earlier read, and returns that read's byte length: the caller pastes right after
// it, so it is the pre-paste offset for the receipt check. A read that shows a
// discontinuity or new activity is never a baseline; the gate keeps waiting within
// the deadline until a later read passes both.
fn wait_for_startup_readiness_with<L, C>(
    read_log: &mut L,
    deadline: Instant,
    timing: ReadinessTiming,
    poll_interval: Duration,
    clock: &mut C,
) -> Result<usize>
where
    L: FnMut() -> Result<Option<Vec<u8>>>,
    C: Clock,
{
    let mut gate = ReadinessGate::new(clock.now(), timing);
    loop {
        let log = read_log().context("Agy startup readiness could not be observed")?;
        let now = clock.now();
        let state = gate.observe(log.as_deref(), now);
        if state == ReadinessState::Ready
            && let Some(observed_len) = gate.observed_len()
        {
            return Ok(observed_len);
        }
        if now >= deadline {
            bail!("{}", gate.deadline_report(state));
        }
        clock.sleep(deadline.saturating_duration_since(now).min(poll_interval));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InputReceipt {
    text: String,
    truncated: bool,
}

// Agy logs the accepted input Go-quoted (`%q`): a complete record ends with the
// closing quote. Anything else, or a trailing ellipsis, is reported as truncated for
// diagnostics; matching still requires the complete marker to be visible.
fn parse_input_receipt(line: &str) -> Option<InputReceipt> {
    let (_, quoted) = line.split_once(INPUT_RECEIPT_MARKER)?;
    let (text, truncated) = match quoted.strip_suffix('"') {
        Some(text) => (text, false),
        None => (quoted, true),
    };
    let (text, truncated) = match text.strip_suffix("...") {
        Some(text) => (text, true),
        None => (text, truncated),
    };
    Some(InputReceipt {
        text: text.to_owned(),
        truncated,
    })
}

fn input_receipts(log: &[u8]) -> Vec<InputReceipt> {
    complete_log_lines(log)
        .filter_map(|line| parse_input_receipt(&line))
        .collect()
}

// The complete marker (`<!-- agent-bridge-agy-turn:<token> -->`) is unique to this
// submission; a visible prefix is not, because another turn's token can share it. A
// receipt that Agy cut before the closing ` -->` therefore never confirms delivery.
// The framing header proves the record is the adapter's own paste rather than a line
// typed into the composer that happens to quote the marker; either adapter-owned
// framing (the Windows JSON envelope, the macOS verbatim prompt) qualifies.
fn receipt_matches(receipt: &InputReceipt, pending: &PendingAgyTurn) -> bool {
    (receipt.text.contains(WINDOWS_PROTOCOL_PREFIX) || receipt.text.contains(TURN_PROTOCOL_HEADER))
        && receipt.text.contains(&pending.marker)
}

// Byte offset of the first line that begins at or after the pre-paste offset. A line
// that straddles the offset started before the paste and is never evidence.
fn evidence_start(log: &[u8], pre_paste_len: usize) -> Option<usize> {
    if pre_paste_len == 0 || log[..pre_paste_len].ends_with(b"\n") {
        return Some(pre_paste_len);
    }
    log[pre_paste_len..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|newline| pre_paste_len + newline + 1)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReceiptEvidence {
    Delivered,
    LogMissing,
    LogShrunk { len: usize },
    NoReceipt { appended: usize, partial_tail: bool },
}

fn observe_input_receipt(
    log: Option<&[u8]>,
    pre_paste_len: usize,
    pending: &PendingAgyTurn,
) -> ReceiptEvidence {
    let Some(log) = log else {
        return ReceiptEvidence::LogMissing;
    };
    if log.len() < pre_paste_len {
        return ReceiptEvidence::LogShrunk { len: log.len() };
    }
    if let Some(start) = evidence_start(log, pre_paste_len)
        && complete_log_lines(&log[start..])
            .filter_map(|line| parse_input_receipt(&line))
            .any(|receipt| receipt_matches(&receipt, pending))
    {
        return ReceiptEvidence::Delivered;
    }
    ReceiptEvidence::NoReceipt {
        appended: log.len() - pre_paste_len,
        partial_tail: log.last().is_some_and(|byte| *byte != b'\n'),
    }
}

fn unconfirmed_receipt_error(
    evidence: &ReceiptEvidence,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    elapsed: Duration,
    full_window: bool,
) -> anyhow::Error {
    let reason = match evidence {
        ReceiptEvidence::Delivered => unreachable!("a delivered receipt is not unconfirmed"),
        ReceiptEvidence::LogMissing => "agy.log is missing after the paste".to_owned(),
        ReceiptEvidence::LogShrunk { len } => format!(
            "agy.log shrank to {len} bytes below the pre-paste offset {pre_paste_len} (rotated or truncated), so the receipt may have been lost"
        ),
        ReceiptEvidence::NoReceipt {
            appended,
            partial_tail,
        } => {
            let observed = format!(
                "no HandleUserInput receipt in the {appended} bytes appended after the pre-paste offset {pre_paste_len}"
            );
            if !full_window {
                format!(
                    "{observed}; the deadline ended the receipt window after {} of the {} second window",
                    elapsed.as_secs(),
                    INPUT_RECEIPT_WINDOW.as_secs()
                )
            } else if *partial_tail {
                format!(
                    "{observed}; agy.log ends with a partial line that may still become the receipt"
                )
            } else {
                format!(
                    "{observed} within {} seconds; Agy logs no marker that proves the console input was drained without a receipt, so non-delivery cannot be proven",
                    elapsed.as_secs()
                )
            }
        }
    };
    anyhow::anyhow!(
        "Agy input receipt for turn marker {} was not confirmed: {reason}; the console paste may have been accepted and is not repeated",
        pending.marker
    )
}

fn confirm_input_receipt_with<L, C>(
    read_log: &mut L,
    pending: &PendingAgyTurn,
    pre_paste_len: usize,
    pasted_at: Instant,
    deadline: Instant,
    poll_interval: Duration,
    clock: &mut C,
) -> terminal::TerminalSendResult
where
    L: FnMut() -> Result<Option<Vec<u8>>>,
    C: Clock,
{
    use terminal::TerminalSendFailure;
    let window_end = input_receipt_window_end(pasted_at, deadline);
    let full_window = window_end.saturating_duration_since(pasted_at) >= INPUT_RECEIPT_WINDOW;
    loop {
        let log = match read_log() {
            Ok(log) => log,
            Err(error) => {
                return Err(TerminalSendFailure::delivery_uncertain(error.context(
                    format!(
                        "Agy input receipt for turn marker {} could not be verified because agy.log is unreadable; the console paste may have been accepted and is not repeated",
                        pending.marker
                    ),
                )));
            }
        };
        let evidence = observe_input_receipt(log.as_deref(), pre_paste_len, pending);
        let now = clock.now();
        let elapsed = now.saturating_duration_since(pasted_at);
        match evidence {
            ReceiptEvidence::Delivered => return Ok(()),
            ReceiptEvidence::LogShrunk { .. } => {
                return Err(TerminalSendFailure::delivery_uncertain(
                    unconfirmed_receipt_error(
                        &evidence,
                        pending,
                        pre_paste_len,
                        elapsed,
                        full_window,
                    ),
                ));
            }
            ReceiptEvidence::LogMissing | ReceiptEvidence::NoReceipt { .. } => {}
        }
        if now >= window_end {
            return Err(TerminalSendFailure::delivery_uncertain(
                unconfirmed_receipt_error(&evidence, pending, pre_paste_len, elapsed, full_window),
            ));
        }
        clock.sleep(window_end.saturating_duration_since(now).min(poll_interval));
    }
}

fn workspace_trust_check(workspace: &Path, directory: &Path) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::Unknown, Check};
    match super::super::consent::Homes::current() {
        Ok(homes) => workspace_trust_check_with(workspace, &homes, directory),
        Err(error) => Check::new(
            "agy_workspace_trust",
            Unknown,
            "agy_trust_store_unlocated",
            format!("{error:#}"),
            "Inspect the managed terminal; doctor never answers the dialog.",
        ),
    }
}

fn workspace_trust_check_with(
    workspace: &Path,
    homes: &super::super::consent::Homes,
    directory: &Path,
) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::*, Check};
    const OBSERVATION: &str =
        "Observation only: doctor never answers the dialog or edits Agy's settings.";
    const WITHHELD: &str = "The Windows initial paste and every macOS follow-up are withheld meanwhile, because a paste onto the dialog is lost and its Enter would approve the folder; an argument-delivered initial prompt still runs behind the dialog.";
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let (availability, reason, detail) = match workspace_trust_missing(workspace, homes, &log_path) {
        Ok(None) => (
            Available,
            "agy_workspace_trusted",
            "Agy's own trust store lists this exact workspace and this session's agy.log shows that it loaded the workspace customizations, so no workspace-trust dialog covers the composer.".to_owned(),
        ),
        Ok(Some(TRUST_STORE_MISSING)) => (
            Unavailable,
            "agy_workspace_untrusted",
            format!("Agy's own trust store does not list this exact workspace, and Agy keeps its workspace-trust dialog over the composer until such a workspace is approved. {WITHHELD}"),
        ),
        Ok(Some(_)) => (
            Unknown,
            "agy_session_trust_unverified",
            format!("Agy's own trust store lists this exact workspace, but this session's agy.log does not show that this session loaded the workspace customizations. Either this session still shows its own workspace-trust dialog, as it does after the workspace was approved in another session, or the log is missing or no longer holds that line. {WITHHELD}"),
        ),
        Err(error) => (
            Unknown,
            "agy_trust_evidence_unreadable",
            format!("Agy's trust store or this session's agy.log could not be read, so the dialog state is unobserved: {error:#}"),
        ),
    };
    let next_action = if availability == Available {
        OBSERVATION.to_owned()
    } else {
        format!(
            "{OBSERVATION} {TRUST_RECOVERY}. A workspace another provider already trusts is answered from shared consent on a later ask."
        )
    };
    Check::new(
        "agy_workspace_trust",
        availability,
        reason,
        detail,
        next_action,
    )
    .evidence(serde_json::json!({ "workspace": workspace, "store": homes.agy, "log": log_path }))
}

fn input_receipt_check(directory: Option<&Path>) -> super::super::doctor::Check {
    input_receipt_check_for_platform(directory, cfg!(windows))
}

fn input_receipt_check_for_platform(
    directory: Option<&Path>,
    windows: bool,
) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::Unknown, Check};
    const CHECK_ID: &str = "agy_input_receipt";
    const NEXT_ACTION: &str = "Observation only. A gated paste (the Windows initial prompt, every macOS follow-up) first waits until Agy's own trust store lists the exact workspace and this session's agy.log shows the workspace customization load (agy_workspace_trust), then for CLI startup completed and a 3.5 s quiet period without reload, redraw, or hooks lines. The Windows console also requires a Full redraw completed after startup. Neither platform waits for the trust reload (Reloading system slash commands and skills stamped at least 1 s after startup and not within 1 s after a Starting new conversation line): Agy logs it only when a trust dialog is approved, so it never comes once the trust evidence holds. The log evidence is re-measured from the new content whenever agy.log disappears, shrinks, or no longer holds the bytes observed earlier (any number of times within the timeout), and the paste follows the read that passed the gate with that read's length as the receipt offset; every paste requires a HandleUserInput receipt carrying the complete pending turn marker within 60 s of the paste (Agy logs the receipt when it processes the paste, which took 18 s under load); a missing receipt leaves delivery uncertain with the session working and the reason in status.error, and the paste is never repeated. Inspect the session and close it explicitly or launch a new one.";
    let redraw_required = windows;
    let startup_markers = if windows {
        "CLI startup completed followed by a Full redraw completed"
    } else {
        "CLI startup completed; macOS logs no Full redraw completed and none is required"
    };
    let Some(directory) = directory else {
        return Check::new(
            CHECK_ID,
            Unknown,
            "agy_log_unavailable",
            "No session was given, so no agy.log startup readiness or input receipt can be observed.",
            NEXT_ACTION,
        );
    };
    let log_path = Reader::open_unchecked(directory)
        .private(AGY_LOG_FILE)
        .path()
        .to_owned();
    let log = match read_log_bytes(&log_path) {
        Ok(Some(log)) => log,
        Ok(None) => {
            return Check::new(
                CHECK_ID,
                Unknown,
                "agy_log_missing",
                "The session has no agy.log yet; startup readiness and input receipts are unobserved.",
                NEXT_ACTION,
            )
            .evidence(serde_json::json!({ "log": log_path }));
        }
        Err(error) => {
            return Check::new(
                CHECK_ID,
                Unknown,
                "agy_log_unreadable",
                format!("{error:#}"),
                NEXT_ACTION,
            )
            .evidence(serde_json::json!({ "log": log_path }));
        }
    };
    let startup = observe_startup(&log);
    let receipts = input_receipts(&log);
    let pending = read_pending_turn(directory).ok().flatten();
    let last = receipts.last();
    // The doctor has no pre-paste offset, so this only states whether the complete
    // marker appears anywhere in the log.
    let pending_received = pending.as_ref().map(|pending| {
        receipts
            .iter()
            .any(|receipt| receipt_matches(receipt, pending))
    });
    // The doctor reads the log once, so it reports the two startup markers only; the
    // quiet period is timed live by the paste gate and cannot be judged here.
    let gate_rule = if windows {
        "the paste gate also waits for a 3.5 s quiet period without reload, redraw, or hooks lines, after the workspace trust evidence (agy_workspace_trust)"
    } else {
        "the follow-up gate also waits for a 3.5 s quiet period without reload or hooks lines, after the workspace trust evidence (agy_workspace_trust)"
    };
    let (reason, detail) = match (startup.markers_observed(redraw_required), last) {
        (false, _) => (
            "agy_startup_markers_missing",
            format!("agy.log does not yet show the startup markers ({startup_markers}); {gate_rule}."),
        ),
        (true, None) => (
            "agy_no_input_receipt",
            "agy.log shows the startup markers but no HandleUserInput receipt.".to_owned(),
        ),
        (true, Some(_)) => (
            "agy_input_receipt_observed",
            "agy.log shows the startup markers and at least one HandleUserInput receipt; the evidence states whether any receipt carries the complete pending turn marker.".to_owned(),
        ),
    };
    Check::new(CHECK_ID, Unknown, reason, detail, NEXT_ACTION).evidence(serde_json::json!({
        "log": log_path,
        "startup_completed": startup.startup_completed(),
        "redraw_required": redraw_required,
        "redraw_after_startup_observed": startup.redraw_after_startup.is_some(),
        "deferred_reload_after_startup_observed": startup.deferred_reload_after_startup.is_some(),
        "startup_reload_after_startup_ignored": startup.startup_reload_after_startup.is_some(),
        "conversation_reload_after_startup_ignored": startup.conversation_reload_after_startup.is_some(),
        "activity_lines": startup.activity_lines,
        "input_receipts": receipts.len(),
        "last_receipt_text": last.map(|receipt| {
            agent_bridge::terminal_safe_text(&receipt.text.chars().take(160).collect::<String>(), false)
        }),
        "last_receipt_truncated": last.map(|receipt| receipt.truncated),
        "pending_marker": pending.as_ref().map(|pending| pending.marker.as_str()),
        "pending_marker_received": pending_received,
    }))
}

pub(super) struct AgyMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl AgyMonitor {
    pub(super) fn start(directory: &Path, log_path: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let log_path = log_path.to_owned();
        let brain_root = brain_root()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-agy-monitor".to_owned())
            .spawn(move || {
                let result = monitor_session(&directory, &log_path, &brain_root, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = turn::Report::monitor_failure(
                        &Store::open_unchecked(&error_directory),
                        FirstPartyCli::Agy,
                        &format!("Agy result monitor failed: {error:#}"),
                    );
                }
                result
            })
            .context("failed to start Agy result monitor")?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub(super) fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .context("Agy result monitor handle was already consumed")?
            .join()
            .map_err(|_| anyhow::anyhow!("Agy result monitor panicked"))??;
        Ok(())
    }
}

impl Drop for AgyMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct TranscriptCursor {
    path: PathBuf,
    full_path: PathBuf,
    offset: u64,
    partial_line: Vec<u8>,
    pending_results: VecDeque<PlannerResult>,
    greatest_result_step: Option<u64>,
}

impl TranscriptCursor {
    fn new(path: PathBuf) -> Self {
        let full_path = path.with_file_name(FULL_TRANSCRIPT_FILE);
        Self {
            path,
            full_path,
            offset: 0,
            partial_line: Vec::new(),
            pending_results: VecDeque::new(),
            greatest_result_step: None,
        }
    }

    fn poll(&mut self, directory: &Path, brain_root: &Path, conversation_id: &str) -> Result<()> {
        let Some(metadata) = validated_file_metadata(&self.path, brain_root)? else {
            return Ok(());
        };
        if metadata.len() < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.pending_results.clear();
        }

        let mut file = RecordReader::at(&self.path).open()?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.partial_line.extend_from_slice(&bytes);

        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in self.partial_line.iter().enumerate() {
            if *byte == b'\n' {
                lines.push(String::from_utf8_lossy(&self.partial_line[start..index]).into_owned());
                start = index + 1;
            }
        }
        if start > 0 {
            self.partial_line.drain(..start);
        }

        for line in lines {
            let Some(result) = parse_planner_result(&line) else {
                continue;
            };
            if self
                .greatest_result_step
                .is_some_and(|previous| result.step <= previous)
                || self
                    .pending_results
                    .iter()
                    .any(|pending| pending.step == result.step)
            {
                continue;
            }
            self.pending_results.push_back(result);
        }

        while let Some(result) = self.pending_results.front() {
            let step = result.step;
            let evidence = ResultReader {
                full_path: &self.full_path,
                brain_root,
            }
            .observe(result, || read_pending_turn(directory))?;
            match evidence {
                ResultEvidence::Incomplete => break,
                ResultEvidence::Unrelated => (),
                ResultEvidence::Correlated {
                    claim_token,
                    message,
                } => {
                    turn::Report::for_claim(
                        &Store::open_unchecked(directory),
                        FirstPartyCli::Agy,
                        Some(&claim_token),
                    )
                    .complete(
                        &message,
                        Some(conversation_id.to_owned()),
                        Some(step.to_string()),
                    )
                    .context("failed to record the correlated Agy result")?;
                }
            }
            self.greatest_result_step = Some(step);
            self.pending_results.pop_front();
        }
        Ok(())
    }
}

/// Read-only interpretation shared by the incremental monitor and a one-shot diagnostic.
/// Full-result lookup precedes the pending-turn read, just as it does in the monitor:
/// a truncated row whose full body has not arrived must not consume or bind a turn.
struct ResultReader<'a> {
    full_path: &'a Path,
    brain_root: &'a Path,
}

enum ResultEvidence {
    Incomplete,
    Unrelated,
    Correlated {
        claim_token: String,
        message: String,
    },
}

impl ResultReader<'_> {
    fn observe(
        &self,
        result: &PlannerResult,
        read_pending: impl FnOnce() -> Result<Option<PendingAgyTurn>>,
    ) -> Result<ResultEvidence> {
        let message = if result.truncated {
            let Some(message) = read_full_result(self.full_path, self.brain_root, result.step)?
            else {
                return Ok(ResultEvidence::Incomplete);
            };
            message
        } else {
            result.message.clone()
        };
        let Some(pending) = read_pending()? else {
            return Ok(ResultEvidence::Unrelated);
        };
        let Ok(message) = correlated_response(&message, &pending) else {
            return Ok(ResultEvidence::Unrelated);
        };
        Ok(ResultEvidence::Correlated {
            message: message.to_owned(),
            claim_token: pending.claim_token,
        })
    }

    fn contains(&self, paths: &[PathBuf], pending: &PendingAgyTurn) -> Result<bool> {
        for path in paths {
            if validated_file_metadata(path, self.brain_root)?.is_none() {
                continue;
            }
            if let Some(text) = RecordReader::at(path).text()? {
                for line in text.lines() {
                    if let Some(result) = parse_planner_result(line)
                        && matches!(
                            self.observe(&result, || Ok(Some(pending.clone())))?,
                            ResultEvidence::Correlated { .. }
                        )
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

fn validated_file_metadata(path: &Path, brain_root: &Path) -> Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular Agy transcript: {}", path.display());
    }
    let canonical_root = brain_root
        .canonicalize()
        .context("Agy brain directory is unavailable")?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("Agy transcript cannot be resolved: {}", path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!(
            "refusing Agy transcript outside its data directory: {}",
            path.display()
        );
    }
    Ok(Some(metadata))
}

fn read_full_result(path: &Path, brain_root: &Path, step: u64) -> Result<Option<String>> {
    if validated_file_metadata(path, brain_root)?.is_none() {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read full Agy transcript: {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("step_index").and_then(serde_json::Value::as_u64) != Some(step) {
            continue;
        }
        let result = parse_planner_value(&value)
            .context("full Agy transcript row did not contain a final response")?;
        if result.truncated {
            bail!("full Agy transcript unexpectedly marked step {step} as truncated");
        }
        return Ok(Some(result.message));
    }
    Ok(None)
}

// A turn Agy gives up on never reaches the transcript as a result: the conversation
// keeps only its USER_INPUT step and agy.log gets one `agent executor error:` line
// (2026-10-01, Agy 1.2.14: `generating and executing: RESOURCE_EXHAUSTED (code 429):
// Individual quota reached. ... Resets in 2h22m28s.`; fifteen sessions, the only such
// lines in more than 100 logs, each of which waited out its whole timeout). Turns are
// sequential and each starts with a `Forwarding user message` line, so an error after
// the newest such line ended the newest turn. That turn is the pending one when the
// log itself says so: a pasted turn is preceded by the receipt that carries its
// marker. The argument-delivered initial prompt has no receipt; it is the first turn
// of the log, and Agy's full transcript must hold exactly one USER_INPUT step, the
// one with the marker, so that a later turn whose log lines are not yet written
// cannot inherit the first turn's error. Replace this when Agy exposes a per-turn
// failure callback with turn identity.
const TURN_START_MARKER: &str = "Forwarding user message to conversation ";
const TURN_FAILURE_MARKER: &str = "agent executor error: ";

// Both failure and approval observations use the same newest-turn binding. The
// argument-delivered first turn still needs the full transcript's sole input.
#[derive(Default)]
struct PendingTurnLog {
    pasted: bool,
    failure: Option<String>,
    confirmation: Option<String>,
}

fn pending_turn_log(log: &[u8], pending: &PendingAgyTurn) -> PendingTurnLog {
    let mut turns = 0_usize;
    let mut receipt = None;
    let mut ours = false;
    let mut observation = PendingTurnLog::default();
    for line in complete_log_lines(log) {
        if let Some(input) = parse_input_receipt(&line) {
            receipt = Some(receipt_matches(&input, pending));
        } else if line.contains(TURN_START_MARKER) {
            let (matches, pasted) = match receipt.take() {
                Some(matches) => (matches, true),
                None => (turns == 0, false),
            };
            ours = matches;
            turns += 1;
            observation = PendingTurnLog {
                pasted,
                ..Default::default()
            };
        } else if ours {
            if let Some((_, error)) = line.split_once(TURN_FAILURE_MARKER) {
                observation
                    .failure
                    .get_or_insert_with(|| error.trim().to_owned());
            } else if let Some(tool) = parse_tool_confirmation(&line) {
                observation.confirmation = Some(tool.to_owned());
            }
        }
    }
    observation
}

fn pending_turn_failure(log: &[u8], pending: &PendingAgyTurn) -> Option<(String, bool)> {
    let observation = pending_turn_log(log, pending);
    observation.failure.map(|error| (error, observation.pasted))
}

// Observed once: Agy 1.3.0, 2026-10-07, issue #82. The line identifies a tool,
// not its command or a documented approval lifecycle. Do not infer approval from
// unrelated later log activity; this is a log observation, not a failure signal.
fn parse_tool_confirmation(line: &str) -> Option<&str> {
    let (_, tail) = line.split_once("Surfacing tool confirmation: \"")?;
    let (tool, step) = tail.split_once("\" at step ")?;
    (!tool.is_empty()
        && tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !step.is_empty()
        && step.bytes().all(|c| c.is_ascii_digit()))
    .then_some(tool)
}

fn tool_confirmation_check(directory: &Path) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::Unknown, Check};
    let observation = brain_root().and_then(|brain| pending_tool_confirmation(directory, &brain));
    let (reason, detail, evidence) = match observation {
        Ok(Some((tool, request_id))) => (
            "agy_tool_confirmation_observed",
            format!(
                "This session's {AGY_LOG_FILE} shows the pending turn waiting for user approval of {tool} in the terminal. Bridge does not answer it. This is the last observed confirmation, not proof that the dialog is still open."
            ),
            serde_json::json!({"log": directory.join(AGY_LOG_FILE), "tool": tool, "request_id": request_id}),
        ),
        Ok(None) => (
            "agy_tool_confirmation_unobserved",
            format!(
                "No tool confirmation is attributable to the pending turn in this session's {AGY_LOG_FILE}."
            ),
            serde_json::Value::Null,
        ),
        Err(error) => (
            "agy_tool_confirmation_unverified",
            format!("The pending turn's tool confirmation could not be verified: {error:#}"),
            serde_json::Value::Null,
        ),
    };
    Check::new("agy_tool_confirmation", Unknown, reason, detail,
        "Observation only. Review any approval prompt in the managed terminal yourself; Bridge does not approve, resend, or release the turn claim.")
        .evidence(evidence)
}

fn pending_tool_confirmation(directory: &Path, brain: &Path) -> Result<Option<(String, String)>> {
    let reader = Reader::open_unchecked(directory);
    let snapshot = super::super::query::observe_snapshot(&reader)?;
    let Some(pending) = read_pending_turn(directory)? else {
        return Ok(None);
    };
    if snapshot.status.state != SessionState::Working
        || snapshot.status.error.is_some()
        || snapshot.pending.is_some()
        || snapshot.claim.as_deref() != Some(&pending.claim_token)
    {
        return Ok(None);
    }
    let Some(receipt) = snapshot
        .receipts
        .iter()
        .find(|r| r.claim_token == pending.claim_token)
    else {
        return Ok(None);
    };
    let Some(log) = reader.private(AGY_LOG_FILE).text()? else {
        return Ok(None);
    };
    let observed = pending_turn_log(log.as_bytes(), &pending);
    let Some(tool) = observed.confirmation.filter(|_| observed.failure.is_none()) else {
        return Ok(None);
    };
    let Some(id) = parse_conversation_id(&log) else {
        return Ok(None);
    };
    let logs = brain.join(id).join(".system_generated").join("logs");
    let full = logs.join(FULL_TRANSCRIPT_FILE);
    if !observed.pasted && !only_user_input_carries(&full, brain, &pending.marker)? {
        return Ok(None);
    }
    // A transcript result may precede the monitor's publication. Observe it without
    // running the monitor or publishing anything, using its parser and correlation.
    if (ResultReader {
        full_path: &full,
        brain_root: brain,
    })
    .contains(&[logs.join(TRANSCRIPT_FILE), full.clone()], &pending)?
    {
        return Ok(None);
    }
    Ok(Some((tool, receipt.request_id.clone())))
}

fn only_user_input_carries(path: &Path, brain_root: &Path, marker: &str) -> Result<bool> {
    if validated_file_metadata(path, brain_root)?.is_none() {
        return Ok(false);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read full Agy transcript: {}", path.display()))?;
    let (mut inputs, mut carries) = (0_usize, false);
    for line in BufReader::new(file).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line?) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("USER_INPUT") {
            inputs += 1;
            carries = value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains(marker));
        }
    }
    Ok(inputs == 1 && carries)
}

#[derive(Default)]
struct MonitorState {
    conversation_id: Option<String>,
    transcript: Option<TranscriptCursor>,
    // The claim whose turn failure has been recorded; its pending-turn file stays
    // until the next turn replaces it.
    failed_claim: Option<String>,
}

impl MonitorState {
    fn poll(&mut self, directory: &Path, log_path: &Path, brain_root: &Path) -> Result<()> {
        let log = RecordReader::at(log_path).text()?;
        if let Some(log) = &log
            && let Some(newest_id) = parse_conversation_id(log)
            && self.conversation_id.as_deref() != Some(newest_id.as_str())
        {
            let path = brain_root
                .join(&newest_id)
                .join(".system_generated")
                .join("logs")
                .join(TRANSCRIPT_FILE);
            self.conversation_id = Some(newest_id);
            self.transcript = Some(TranscriptCursor::new(path));
        }
        if let (Some(id), Some(cursor)) =
            (self.conversation_id.as_deref(), self.transcript.as_mut())
        {
            // A result Agy has already written is recorded first, so a failure is
            // only attributed to a turn that has none at this point. The failure
            // releases the claim: a result written afterwards is not accepted.
            cursor.poll(directory, brain_root, id)?;
            if let Some(log) = &log
                && let Some(pending) = read_pending_turn(directory)?
                && self.failed_claim.as_deref() != Some(pending.claim_token.as_str())
                && let Some((error, pasted)) = pending_turn_failure(log.as_bytes(), &pending)
                && (pasted
                    || only_user_input_carries(&cursor.full_path, brain_root, &pending.marker)?)
            {
                turn::Report::for_claim(
                    &Store::open_unchecked(directory),
                    FirstPartyCli::Agy,
                    Some(&pending.claim_token),
                )
                .fail(
                    &format!("Agy turn failed: {error}"),
                    Some(id.to_owned()),
                    None,
                )
                .context("failed to record the Agy turn failure")?;
                self.failed_claim = Some(pending.claim_token);
            }
        }
        Ok(())
    }
}

fn monitor_session(
    directory: &Path,
    log_path: &Path,
    brain_root: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let mut state = MonitorState::default();
    loop {
        state.poll(directory, log_path, brain_root)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn brain_root() -> Result<PathBuf> {
    default_brain_root(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn default_brain_root(
    home: Option<&std::ffi::OsStr>,
    user_profile: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let home = PathBuf::from(
        home.or(user_profile)
            .context("neither HOME nor USERPROFILE is set for the Agy brain root")?,
    );
    Ok(home.join(".gemini").join("antigravity-cli").join("brain"))
}

fn parse_conversation_id(log: &str) -> Option<String> {
    log.lines().rev().find_map(|line| {
        let (_, suffix) = line.rsplit_once("Created conversation ")?;
        let candidate = suffix.split_whitespace().next()?;
        valid_uuid(candidate).then(|| candidate.to_owned())
    })
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Debug)]
struct PlannerResult {
    step: u64,
    message: String,
    truncated: bool,
}

fn parse_planner_result(line: &str) -> Option<PlannerResult> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    parse_planner_value(&value)
}

fn parse_planner_value(value: &serde_json::Value) -> Option<PlannerResult> {
    if value.get("type")?.as_str()? != "PLANNER_RESPONSE"
        || value.get("status")?.as_str()? != "DONE"
        || value.get("source")?.as_str()? != "MODEL"
    {
        return None;
    }
    if value
        .get("tool_calls")
        .is_some_and(|tool_calls| match tool_calls {
            serde_json::Value::Null => false,
            serde_json::Value::Array(calls) => !calls.is_empty(),
            _ => true,
        })
    {
        return None;
    }
    let step = value.get("step_index")?.as_u64()?;
    let message = value.get("content")?.as_str()?.trim();
    (!message.is_empty()).then(|| PlannerResult {
        step,
        message: message.to_owned(),
        truncated: value
            .get("is_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
fn parse_transcript_line(line: &str) -> Option<(u64, String)> {
    let result = parse_planner_result(line)?;
    (!result.truncated).then_some((result.step, result.message))
}

#[cfg(test)]
mod tests {
    #[test]
    fn paste_preparation_requires_trust_correlation_and_readiness_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let readiness_reads = std::cell::Cell::new(0);
        let readiness = || {
            readiness_reads.set(readiness_reads.get() + 1);
            Ok(26060)
        };
        assert!(
            PreparedTerminalTurn::prepare_with(
                tmp.path(),
                || bail!("session ended during trust wait"),
                readiness
            )
            .is_err()
        );
        assert_eq!(readiness_reads.get(), 0);
        assert!(PreparedTerminalTurn::prepare_with(tmp.path(), || Ok(()), readiness).is_err());
        assert_eq!(
            readiness_reads.get(),
            0,
            "missing correlation must prevent a paste"
        );
        install_pending_turn(tmp.path(), "1-2-3").unwrap();
        assert!(
            PreparedTerminalTurn::prepare_with(tmp.path(), || Ok(()), || bail!("not ready"))
                .is_err()
        );
        assert!(!tmp.path().join("agy-input-1-2-3.json").exists());
    }

    #[test]
    fn prepared_paste_uses_the_ready_read_offset_once_and_never_retries() {
        let tmp = tempfile::tempdir().unwrap();
        install_pending_turn(tmp.path(), "1-2-3").unwrap();
        for failure in ["none", "not-sent", "send-uncertain", "receipt-uncertain"] {
            let order = std::cell::RefCell::new(Vec::new());
            let prepared = PreparedTerminalTurn::prepare_with(
                tmp.path(),
                || {
                    order.borrow_mut().push("trust");
                    Ok(())
                },
                || {
                    order.borrow_mut().push("ready-read");
                    Ok(26060)
                },
            )
            .unwrap();
            let result = prepared.deliver(
                "iterm2",
                || {
                    order.borrow_mut().push("paste");
                    match failure {
                        "not-sent" => Err(terminal::TerminalSendFailure::not_sent(
                            anyhow::anyhow!("not sent"),
                        )),
                        "send-uncertain" => Err(terminal::TerminalSendFailure::delivery_uncertain(
                            anyhow::anyhow!("uncertain"),
                        )),
                        _ => Ok(()),
                    }
                },
                |pending, offset| {
                    order.borrow_mut().push("receipt");
                    assert_eq!(offset, 26060);
                    assert_eq!(pending.claim_token, "1-2-3");
                    if failure == "receipt-uncertain" {
                        Err(terminal::TerminalSendFailure::delivery_uncertain(
                            anyhow::anyhow!("no receipt"),
                        ))
                    } else {
                        Ok(())
                    }
                },
            );
            let expected = if matches!(failure, "not-sent" | "send-uncertain") {
                vec!["trust", "ready-read", "paste"]
            } else {
                vec!["trust", "ready-read", "paste", "receipt"]
            };
            assert_eq!(*order.borrow(), expected);
            match failure {
                "none" => result.unwrap(),
                "not-sent" => assert!(!result.unwrap_err().delivery_may_have_occurred()),
                _ => assert!(result.unwrap_err().delivery_may_have_occurred()),
            }
        }
    }

    #[test]
    fn missing_receipt_is_traced_once_and_trace_failure_does_not_cancel_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = PendingAgyTurn::new("48-1-0").unwrap();
        let sends = std::cell::Cell::new(0);
        let result = trace_terminal_delivery(
            tmp.path(),
            &pending,
            26060,
            "apple-terminal",
            || {
                sends.set(sends.get() + 1);
                Ok(())
            },
            || {
                Err(terminal::TerminalSendFailure::delivery_uncertain(
                    anyhow::anyhow!("no HandleUserInput receipt"),
                ))
            },
        );
        assert!(result.unwrap_err().delivery_may_have_occurred());
        assert_eq!(sends.get(), 1);
        let trace: serde_json::Value = serde_json::from_slice(
            &std::fs::read(tmp.path().join("agy-input-48-1-0.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(trace["pre_paste_offset"], 26060);
        assert_eq!(trace["outcome"], "delivery-uncertain");
        assert!(
            trace["paste_started_unix_ms"].as_u64().unwrap()
                <= trace["paste_returned_unix_ms"].as_u64().unwrap()
        );
        let blocked = tmp.path().join("not-a-directory");
        std::fs::write(&blocked, b"preserve").unwrap();
        trace_terminal_delivery(
            &blocked,
            &pending,
            10,
            "iterm2",
            || {
                sends.set(sends.get() + 1);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(sends.get(), 2);
        assert_eq!(std::fs::read(&blocked).unwrap(), b"preserve");
        let confirms = std::cell::Cell::new(0);
        let failure = trace_terminal_delivery(
            tmp.path(),
            &pending,
            10,
            "iterm2",
            || {
                Err(terminal::TerminalSendFailure::not_sent(anyhow::anyhow!(
                    "deadline"
                )))
            },
            || {
                confirms.set(confirms.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert_eq!(confirms.get(), 0);
    }

    #[test]
    fn workspace_trust_and_dialog_are_exact_and_do_not_accept_permission_prompts() {
        use super::super::super::consent::{self, Trust};
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let homes = consent::fixture_homes(tmp.path());
        let key = consent::native_key(&workspace).unwrap();
        super::super::super::write_json_atomic(
            &homes.agy,
            &serde_json::json!({"trustedWorkspaces":[key.clone()]}),
        )
        .unwrap();
        assert!(matches!(
            ADAPTER.workspace_trust(&workspace, &homes).unwrap(),
            Trust::Trusted(_)
        ));
        assert_eq!(
            ADAPTER
                .workspace_trust(&workspace.join("child"), &homes)
                .unwrap(),
            Trust::Absent
        );
        let screen = format!(
            "Accessing workspace:\n{key}\nDo you trust the contents of this project?\nAntigravity CLI requires permission to read, edit, and execute files here.\n> Yes, I trust this folder\nNo, exit\n↑/↓ Navigate · enter Confirm\nGemini 3.8 Flash · high"
        );
        assert_eq!(
            agy_trust_prompt_key(&screen, &workspace),
            Some(terminal::DialogKey::Enter)
        );
        // The footer names the saved model, which need not be a Gemini one, and it
        // is absent in the first second (session-a1QguW and session-n3tsmp).
        for footer in ["Claude Opus 4.6 (Thinking)", "GPT-OSS 120B (Medium)"] {
            assert_eq!(
                agy_trust_prompt_key(
                    &screen.replace("Gemini 3.8 Flash · high", footer),
                    &workspace
                ),
                Some(terminal::DialogKey::Enter),
                "{footer}"
            );
        }
        assert_eq!(
            agy_trust_prompt_key(
                screen.trim_end_matches("\nGemini 3.8 Flash · high"),
                &workspace
            ),
            Some(terminal::DialogKey::Enter)
        );
        assert_eq!(
            agy_trust_prompt_key(&screen, &workspace.join("child")),
            None
        );
        assert_eq!(
            agy_trust_prompt_key(&(screen.clone() + "\nAllow terminal command?"), &workspace),
            None
        );
        assert_eq!(
            agy_trust_prompt_key(
                &screen.replace("> Yes, I trust this folder", "Yes, I trust this folder"),
                &workspace
            ),
            None
        );
        std::fs::write(&homes.agy, b"{\"trustedWorkspaces\":true}").unwrap();
        assert!(ADAPTER.workspace_trust(&workspace, &homes).is_err());
    }

    #[test]
    fn diagnostics_describe_agy_owned_fallback_without_claiming_live_delivery() {
        use super::super::super::doctor::{Availability, Context};
        use super::NativeProviderAdapter;
        let checks = super::ADAPTER.diagnose(Context {
            directory: None,
            manifest: None,
            executable: None,
            current_version: None,
            workspace: std::path::Path::new("."),
            probe: false,
            deadline: std::time::Instant::now(),
        });
        assert_eq!(checks[0].reason_code, "agy_terminal_fallback");
        assert_eq!(checks[0].availability, Availability::Unknown);
    }

    use super::super::super::{
        SessionEvent, SessionStatus, acquire_turn_claim, event_paths, read_json, update_status,
    };
    use super::*;
    use std::cell::Cell;
    use std::io::Write;
    use std::rc::Rc;

    fn claim_pending_turn(directory: &Path) -> PendingAgyTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token().to_owned();
        claim.retain();
        install_pending_turn(directory, &token).unwrap()
    }

    fn marked(message: &str, pending: &PendingAgyTurn) -> String {
        format!("{message}\n{}", pending.marker)
    }

    #[test]
    fn windows_console_prompt_preserves_multiline_input_without_raw_submission_keys() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let prompt = "first line\nsecond\tcolumn";
        let framed = terminal_correlated_prompt(prompt, &pending, true).unwrap();

        assert!(
            framed
                .chars()
                .all(|character| !matches!(character, '\r' | '\n' | '\t'))
        );
        assert!(framed.contains(&serde_json::to_string(prompt).unwrap()));
        assert!(framed.contains(&pending.marker));
    }

    fn planner_line(step: u64, content: &str) -> String {
        serde_json::json!({
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "source": "MODEL",
            "step_index": step,
            "content": content,
        })
        .to_string()
    }

    #[test]
    fn log_and_transcript_parsers_accept_only_completed_results() {
        let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
        assert_eq!(
            parse_conversation_id(&format!("prefix Created conversation {id}\n")),
            Some(id.to_owned())
        );
        assert!(parse_conversation_id("Created conversation ../../outside").is_none());

        let completed = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":9,"content":"AGY_TOOL_OK"}"#;
        assert_eq!(
            parse_transcript_line(completed),
            Some((9, "AGY_TOOL_OK".to_owned()))
        );
        let intermediate = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":7,"content":""}"#;
        assert_eq!(parse_transcript_line(intermediate), None);
        let planner_tool = r#"{"type":"PLANNER_RESPONSE","status":"DONE","source":"MODEL","step_index":8,"content":"checking","tool_calls":[{"name":"run_command"}]}"#;
        assert_eq!(parse_transcript_line(planner_tool), None);
        let tool = r#"{"type":"RUN_COMMAND","status":"DONE","source":"MODEL","step_index":8,"content":"output"}"#;
        assert_eq!(parse_transcript_line(tool), None);
    }

    #[test]
    fn agy_rejects_the_native_hook_transport_it_does_not_use() {
        let payload = serde_json::json!({ "last_assistant_message": "unexpected" });
        assert!(ADAPTER.handle_hook(Path::new("unused"), &payload).is_err());
    }

    // Real log excerpts written by Agy 1.2.10 on 2026-09-24 (issue #43), taken from
    // `%USERPROFILE%\.agent-bridge\native-sessions\<session>\agy.log`. Every kept line
    // is verbatim; the listed line numbers are the file positions and the omitted lines
    // are HTTP, auth, model and quota chatter (the auth lines carry the account email
    // and are not reproduced). Timestamps are local time (KST).
    //
    // session-udT6uY (initial paste delivered): lines 91, 100, 101, 119-123, 134, 147
    // and 153 are the startup. The main thread's hooks line (91) precedes the startup
    // reload (120), whose own hooks completion (122) follows at once. That startup
    // reload is stamped 0.5 ms after `CLI startup completed`, so it is the startup
    // reload, not the trust reload, which this trusted workspace never logs.
    const REAL_SUCCESS_STARTUP: &str = r"I0924 16:42:24.068931       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:42:24.073126       1 common.go:438] Starting CLI program
CLI ready for user input
I0924 16:42:24.080467       1 analytics.go:187] CLI startup completed (took 230.2044ms)
I0924 16:42:24.080986     215 manager.go:1331] Reloading system slash commands and skills
I0924 16:42:24.080986     215 manager.go:1308] Reloading system slash commands
I0924 16:42:24.081497     200 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:42:24.127839     402 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 16:42:25.848949     499 manager.go:1308] Reloading system slash commands
I0924 16:42:28.530216     508 manager.go:1308] Reloading system slash commands
I0924 16:42:29.104592     531 manager.go:1308] Reloading system slash commands
";

    // session-udT6uY line 156: the 2,731 byte receipt of the delivered paste, abridged
    // here after the protocol prefix, the marker and the start of the JSON request.
    // The original line continues with the Go-quoted request and ends with `\""`.
    const REAL_SUCCESS_RECEIPT_HEAD: &str = r#"I0924 16:42:50.212542     595 input_loop.go:107] HandleUserInput called with text: "[Agent Bridge Agy Windows console turn protocol] Decode the following JSON string as the complete request, preserving escaped newlines and tabs. Complete it as one turn. End the complete final response with the exact marker <!-- agent-bridge-agy-turn:28404-1790235743098225800-0 --> on its own final line; do not alter or omit it. Request JSON: \"[Agent Bridge native delegation]\\nSource: external"#;

    // session-udT6uY lines 157, 169 and 170: what followed the receipt.
    const REAL_SUCCESS_AFTER_RECEIPT: &str = r"I0924 16:42:50.213057     141 conversation_manager.go:512] Starting new conversation (agent=false)
I0924 16:42:50.228104     580 manager.go:1331] Reloading system slash commands and skills
I0924 16:42:50.228760     580 manager.go:1308] Reloading system slash commands
";

    // session-fMqSQc (initial paste lost): lines 95, 104, 105, 114-116, 126, 127, 138,
    // 150 and 151. The startup reload (114) precedes `CLI startup completed` and
    // skipped its hooks pass, so the only hooks line before 16:41:20 is the main
    // thread's (95). The fixed 12 second delay pasted at about 16:41:19, and the
    // round-3 gate would have been ready at 16:41:17.24, 3.5 s after the 16:41:13
    // reload; both precede the trust reload below, which the paste itself caused.
    const REAL_FAILURE_STARTUP: &str = r"I0924 16:41:07.819578       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:41:07.823186       1 common.go:438] Starting CLI program
CLI ready for user input
I0924 16:41:07.826763     280 manager.go:1331] Reloading system slash commands and skills
I0924 16:41:07.826763     280 manager.go:1308] Reloading system slash commands
I0924 16:41:07.826763     280 manager.go:1312] Slash commands unchanged, skipping update
I0924 16:41:07.830345       1 analytics.go:187] CLI startup completed (took 226.5353ms)
I0924 16:41:07.876154     269 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 16:41:10.732767     345 manager.go:1308] Reloading system slash commands
I0924 16:41:13.737805     383 manager.go:1308] Reloading system slash commands
I0924 16:41:13.739814     383 manager.go:1312] Slash commands unchanged, skipping update
";

    // session-fMqSQc lines 153-156, the end of the file: the deferred skills reload
    // that discarded the paste, 13.0 s after startup, and the first hooks completion
    // after a skills reload.
    // Nothing was logged after line 156 until the session was closed at 16:42:22: no
    // HandleUserInput receipt and no line that shows the console input was drained.
    const REAL_FAILURE_LATE_RELOAD: &str = r"I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills
I0924 16:41:20.813553     410 manager.go:1308] Reloading system slash commands
I0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:41:20.816140     410 manager.go:1312] Slash commands unchanged, skipping update
";

    const REAL_SUCCESS_TOKEN: &str = "28404-1790235743098225800-0";

    // session-IQHEwf (this machine, 2026-09-24 17:20, Agy 1.2.10; the round-1 gate
    // waited the full deadline and never pasted): the startup verbatim after the ANSI
    // strip, with the account email redacted. The startup reload precedes
    // `CLI startup completed` and no hooks_manager.go line follows it, the deferred
    // `... and skills` reload never came, and after three plain reloads within 6.4 s
    // the log was silent for five minutes. A healthy session, so the reload/hooks
    // pair cannot be the readiness discriminator, and the deferred reload cannot be
    // required at all: the gate pastes one quiet period after the last plain reload.
    const REAL_QUIET_STARTUP: &str = r#"E0924 17:20:28.610124     222 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 17:20:28.610124     222 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 17:20:28.610124     222 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 17:20:28.613199     270 manager.go:1331] Reloading system slash commands and skills
I0924 17:20:28.613199     270 manager.go:1308] Reloading system slash commands
I0924 17:20:28.613199     270 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:20:28.614019     232 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 17:30:33.268465 +0900 KST expired=false
I0924 17:20:28.614522     231 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 17:20:28.614522     231 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 17:20:28.614522     231 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 17:20:28.614522     231 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x35cfc40fc0f0) is same as b.cliAuth (0x35cfc40fc0f0)
W0924 17:20:28.615030     239 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:20:28.613199     265 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 17:20:28.615030     265 gemini_extensions.go:49] No extensions found
W0924 17:20:28.615030     204 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:20:28.616574       1 analytics.go:187] CLI startup completed (took 227.1451ms)
I0924 17:20:28.663151     342 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 17:20:29.582022     238 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x1e5c99c8fa1790b0
I0924 17:20:30.023964     235 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x524ece174cc9e1c6
W0924 17:20:30.061976     231 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:30.062976     231 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:30.063573     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:20:30.063573     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:30.480483     249 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 17:20:30.481001     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:30.481001     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:30.481001     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:30.481001     332 manager.go:1308] Reloading system slash commands
I0924 17:20:31.485622     387 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe68c0b26c4c162eb
I0924 17:20:32.802591     387 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x46f67b480998ff4b
W0924 17:20:33.025968     387 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:33.025968     387 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:33.025968     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:33.025968     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
W0924 17:20:33.025968     390 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:20:33.026470     390 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:20:33.155749     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:33.155749     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:33.155749     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:33.155749     249 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:20:33.155749     422 manager.go:1308] Reloading system slash commands
I0924 17:20:33.161144     422 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:20:34.044434     389 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x5dfcd4420938bfd
I0924 17:20:34.759982     248 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:20:34.950075     249 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:20:34.950075     249 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:20:34.950075     249 experiment_manager.go:70] Experiments refreshed after login
I0924 17:20:34.950075     430 manager.go:1308] Reloading system slash commands
I0924 17:20:34.952107     430 manager.go:1312] Slash commands unchanged, skipping update
"#;

    // session-IEKjtC (this machine, 2026-09-24 17:47, Agy 1.2.10; the round-3 build
    // pasted at about 17:47:13.1 and lost the paste): the startup verbatim after the
    // ANSI strip, with the account email redacted. The startup reload precedes `CLI
    // startup completed`; three plain reloads follow within 6 s; the round-3 gate was
    // ready at 17:47:13.12, 3.5 s after the last of them; and the deferred skills
    // reload with its hooks line arrived at 17:47:13.468, 9.8 s after startup,
    // clearing the composer. Only the two 17:53 lines were logged in the six minutes
    // after it: no HandleUserInput receipt, so the adapter reported
    // delivery-uncertain.
    const REAL_DEFERRED_RELOAD_STARTUP: &str = r#"I0924 17:47:03.616815     291 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 17:47:03.616815     296 manager.go:1331] Reloading system slash commands and skills
I0924 17:47:03.617328     296 manager.go:1308] Reloading system slash commands
I0924 17:47:03.617328     296 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:03.617328     291 gemini_extensions.go:49] No extensions found
I0924 17:47:03.617871     141 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 18:30:33.2570659 +0900 KST expired=false
I0924 17:47:03.617871     140 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 17:47:03.617871     140 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 17:47:03.617871     140 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 17:47:03.617871     140 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2246f4b581e0) is same as b.cliAuth (0x2246f4b581e0)
W0924 17:47:03.618378     327 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 17:47:03.618378     311 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 17:47:03.620082       1 analytics.go:187] CLI startup completed (took 223.5116ms)
I0924 17:47:03.666570     377 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 17:47:05.152404     310 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xaf0c6d6cd1ce8304
I0924 17:47:05.675593     323 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x4981a71d4a9d22c2
W0924 17:47:05.713908     140 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:05.713908     140 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:05.713908     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:47:05.714422     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:06.420977     254 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 17:47:06.421536     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:06.421536     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:06.421536     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:06.421536     240 manager.go:1308] Reloading system slash commands
I0924 17:47:07.612954     409 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x873d189845330ceb
I0924 17:47:08.472485     409 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x25fb0d59725338a1
W0924 17:47:08.766467     409 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:08.766467     409 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:08.766467     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:08.766467     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
W0924 17:47:08.766467     433 model_config_manager.go:67] Failed to resolve model flag "gemini-3.8-flash-high": --model gemini-3.8-flash-high conflicts with --effort=low
I0924 17:47:08.766467     433 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 17:47:08.894558     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:08.894558     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:08.894558     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:08.894558     254 experiment_manager.go:66] Starting experiment refresh after login
I0924 17:47:08.894558     450 manager.go:1308] Reloading system slash commands
I0924 17:47:08.896639     450 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:09.223158     253 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 17:47:09.616209     254 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 17:47:09.616209     254 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 17:47:09.616209     254 experiment_manager.go:70] Experiments refreshed after login
I0924 17:47:09.616209     303 manager.go:1308] Reloading system slash commands
I0924 17:47:09.618298     303 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:47:09.685316     432 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x83274a231aa9a1d5
I0924 17:47:13.468286     399 manager.go:1331] Reloading system slash commands and skills
I0924 17:47:13.468286     399 manager.go:1308] Reloading system slash commands
I0924 17:47:13.468806     478 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 17:47:13.470456     399 manager.go:1312] Slash commands unchanged, skipping update
I0924 17:53:04.130429     656 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0xebc1b272b39c997d
I0924 17:53:04.677160     651 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xbcc1674415841087
"#;

    // session-ql5TVc (this machine, 2026-09-24 18:48, Agy 1.2.10; the round-6 build
    // pasted at about 18:48:24.2, when its 20 s window ended, and lost the paste):
    // lines 110-152 (the end of the file) verbatim after the ANSI strip, with the
    // account email redacted. The startup reload precedes `CLI startup completed`;
    // two plain reloads follow within 4.6 s; the quiet period ended at 18:48:12.15;
    // the 20 s window ended at 18:48:24.12 and the gate pasted; and the deferred
    // skills reload with its hooks line arrived at 18:48:25.481988, 21.4 s after
    // startup, clearing the composer. Nothing was logged after it: no
    // HandleUserInput receipt, so the adapter reported delivery-uncertain.
    const REAL_LATE_DEFERRED_RELOAD_STARTUP: &str = r#"I0924 18:48:04.112210     338 manager.go:1331] Reloading system slash commands and skills
I0924 18:48:04.112210     338 manager.go:1308] Reloading system slash commands
I0924 18:48:04.112210     338 manager.go:1312] Slash commands unchanged, skipping update
I0924 18:48:04.112210     141 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 18:48:04.112725     141 gemini_extensions.go:49] No extensions found
I0924 18:48:04.112725     208 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 19:30:33.2676274 +0900 KST expired=false
I0924 18:48:04.113240     207 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 18:48:04.113240     207 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 18:48:04.113240     207 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 18:48:04.113240     207 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x1ecd003d80f0) is same as b.cliAuth (0x1ecd003d80f0)
W0924 18:48:04.114757     217 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 18:48:04.114757     359 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 18:48:04.115440       1 analytics.go:187] CLI startup completed (took 234.4106ms)
I0924 18:48:04.161713      61 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 18:48:05.439653     213 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xb4200852ff66f489
I0924 18:48:05.808465     213 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0xa2c72879b6413d5a
I0924 18:48:05.844379     207 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:05.844379     207 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:05.844959     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 18:48:05.844959     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 18:48:05.977318     246 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 18:48:05.977318     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 18:48:05.977318     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 18:48:05.977318     246 experiment_manager.go:70] Experiments refreshed after login
I0924 18:48:05.977318     326 manager.go:1308] Reloading system slash commands
I0924 18:48:07.328742     260 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x830ddd43c74d8225
I0924 18:48:08.309372     260 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x386208932c049341
I0924 18:48:08.471064     260 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:08.471064     260 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:08.471064     328 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 18:48:08.471064     328 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 18:48:08.471064     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 18:48:08.472576     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 18:48:08.646591     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 18:48:08.646591     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 18:48:08.646591     246 experiment_manager.go:70] Experiments refreshed after login
I0924 18:48:08.646591     270 manager.go:1308] Reloading system slash commands
I0924 18:48:08.649582     270 manager.go:1312] Slash commands unchanged, skipping update
I0924 18:48:09.842042     327 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x20191198b6612470
I0924 18:48:25.481988     379 manager.go:1331] Reloading system slash commands and skills
I0924 18:48:25.481988     379 manager.go:1308] Reloading system slash commands
I0924 18:48:25.482515     375 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 18:48:25.485154     379 manager.go:1312] Slash commands unchanged, skipping update
"#;

    // session-uqraap (this machine, 2026-09-24 20:44, Agy 1.2.10, CPU idle, a second
    // Agy launch overlapping; the round-8 build pasted at about 20:44:36.5, when its
    // 35 s window ended, and lost the paste): lines 96-149 (the end of the file)
    // verbatim after the ANSI strip, with the account email redacted. The startup
    // reload precedes `CLI startup completed`; two plain reloads follow within 5 s;
    // the quiet period ended at 20:44:09.93; the 35 s window ended at 20:44:36.44
    // and the gate pasted; and the deferred skills reload with its hooks line
    // arrived at 20:44:37.855815, 36.4 s after startup, clearing the composer.
    // session-Cf1FBY (20:36, heavy CPU load) logged the same reload at +36.4 s and
    // lost its paste the same way. Nothing was logged after it: no HandleUserInput
    // receipt, so the adapter reported delivery-uncertain.
    const REAL_SECOND_CLUSTER_RELOAD_STARTUP: &str = r#"I0924 20:44:01.434843       1 common.go:438] Starting CLI program
CLI ready for user input
W0924 20:44:01.434843     187 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 20:44:01.434843     187 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 20:44:01.434843     187 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 20:44:01.441059     296 manager.go:1331] Reloading system slash commands and skills
I0924 20:44:01.441059     296 manager.go:1308] Reloading system slash commands
I0924 20:44:01.441059     296 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:44:01.441059     291 gemini_extensions.go:28] Detecting Gemini extensions in C:\Users\user\.gemini\extensions
I0924 20:44:01.441567     291 gemini_extensions.go:49] No extensions found
I0924 20:44:01.442107      75 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 21:30:53.5504966 +0900 KST expired=false
I0924 20:44:01.442107      74 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 20:44:01.442107      74 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 20:44:01.442107      74 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 20:44:01.442107      74 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2a7b00ba00f0) is same as b.cliAuth (0x2a7b00ba00f0)
W0924 20:44:01.442107     322 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 20:44:01.442107     343 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 20:44:01.443214       1 analytics.go:187] CLI startup completed (took 233.4541ms)
I0924 20:44:01.490907     375 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 20:44:03.314578      81 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x64962e4d65df3a83
I0924 20:44:03.945369      78 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x18e64b39b3fcc68a
I0924 20:44:03.987146      74 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:03.987146      74 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:03.987146     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:03.987146     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:44:04.176021     246 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 20:44:04.176021     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:44:04.176021     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:44:04.176021     246 experiment_manager.go:70] Experiments refreshed after login
I0924 20:44:04.176592     429 manager.go:1308] Reloading system slash commands
I0924 20:44:05.022530     422 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe8d8c908fb469f5d
I0924 20:44:06.141949     422 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xf0c9ea8cbfd8e96a
I0924 20:44:06.342958     422 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:06.342958     422 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:06.342958     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:06.342958     439 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:44:06.342958     439 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:44:06.342958     246 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:44:06.427564     246 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:44:06.427564     246 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:44:06.427564     246 experiment_manager.go:70] Experiments refreshed after login
I0924 20:44:06.427564     445 manager.go:1308] Reloading system slash commands
I0924 20:44:06.428584     445 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:44:06.643486     245 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:44:07.314437     438 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xcb0bf3ebb06517e2
I0924 20:44:37.855815     516 manager.go:1331] Reloading system slash commands and skills
I0924 20:44:37.855815     516 manager.go:1308] Reloading system slash commands
I0924 20:44:37.856320     355 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 20:44:37.857983     516 manager.go:1312] Slash commands unchanged, skipping update
"#;

    // session-M8QFPp (this machine, 2026-09-24 20:37, Agy 1.2.10, heavy CPU load;
    // the paste was accepted but reported delivery-uncertain): lines 123-157
    // verbatim after the ANSI strip, with the account email redacted. The startup
    // reload follows `CLI startup completed` by 1.6 ms with its hooks line, and the
    // round-8 rule took it for the deferred reload, so the gate was ready 3.5 s
    // after the last plain reload at 20:37:45.65 and pasted at about 20:37:49.15.
    // The HandleUserInput receipt (line 160, not reproduced) was logged at
    // 20:38:07.152165, 18.0 s after the paste and outside the former 15 s receipt
    // window. No deferred skills reload was logged before it; the skills reload at
    // 20:38:07.160926 is the conversation reload.
    const REAL_STARTUP_RELOAD_AFTER_STARTUP: &str = r#"I0924 20:37:40.653251       1 analytics.go:187] CLI startup completed (took 241.0837ms)
I0924 20:37:40.654897     338 manager.go:1331] Reloading system slash commands and skills
I0924 20:37:40.654897     338 manager.go:1308] Reloading system slash commands
I0924 20:37:40.656148      96 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 20:37:40.700886     142 manager.go:934] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I0924 20:37:42.065160     297 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x4e9df0103680b301
I0924 20:37:42.456755     297 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels Trace: 0x17d08bd806d2a0c2
I0924 20:37:42.496252     293 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:42.496252     293 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:42.496252     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:42.496252     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:42.678109     253 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 20:37:42.678109     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:42.678109     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:42.678109     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:42.678621     411 manager.go:1308] Reloading system slash commands
I0924 20:37:43.588140     288 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xab7203296c667660
I0924 20:37:45.048591     288 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0xe1422278db03c287
I0924 20:37:45.218409     288 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:45.218409     288 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:45.218409     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:45.218409     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:45.218409     483 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 20:37:45.218409     483 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 20:37:45.515277     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:45.515277     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:45.515277     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:45.515277     253 experiment_manager.go:66] Starting experiment refresh after login
I0924 20:37:45.515277     417 manager.go:1308] Reloading system slash commands
I0924 20:37:45.517352     417 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:37:45.646941     253 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 20:37:45.646941     253 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 20:37:45.646941     253 experiment_manager.go:70] Experiments refreshed after login
I0924 20:37:45.646941     498 manager.go:1308] Reloading system slash commands
I0924 20:37:45.649546     498 manager.go:1312] Slash commands unchanged, skipping update
I0924 20:37:45.892979     252 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 20:37:46.098266     482 http_helpers.go:305] URL: https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist Trace: 0x7c00680e8820122f
"#;

    // session-QMFk6F (this Mac, macOS 26.6 + iTerm2, 2026-09-24 21:32, Agy 1.2.10; the
    // live smoke lost its first `tell`): the startup and the argument-delivered initial
    // turn verbatim from line 100 of agy.log, with the account email redacted and the
    // HTTP trace and latency-breakdown lines omitted. Agy logs no `Full redraw
    // completed` line at all on macOS. The startup reload precedes `CLI startup
    // completed`; `Starting new conversation` (+2.9 s) is followed 7 ms later by the
    // conversation reload, which the former rule took for the deferred reload; the
    // trust reload and its hooks line come at +11.2 s. The smoke pasted its
    // follow-up at 21:32:18.79 (+10.3 s) onto the workspace-trust dialog; its Enter
    // approved the folder, Agy logged that reload 0.85 s later, and no
    // `HandleUserInput` receipt followed.
    const REAL_MACOS_INITIAL_TURN: &str = r#"E0924 21:32:08.448630     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
W0924 21:32:08.448718     204 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.448740     204 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 21:32:08.448803     204 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.448822     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 21:32:08.452158     261 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:08.452191     261 manager.go:1308] Reloading system slash commands
I0924 21:32:08.452164     128 gemini_extensions.go:28] Detecting Gemini extensions in /Users/tester/.gemini/extensions
I0924 21:32:08.452226     128 gemini_extensions.go:49] No extensions found
I0924 21:32:08.452205     261 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:08.458864     258 encoder_embed.go:85] Installing/updating embedded webm_encoder binary to /Users/tester/.gemini/antigravity-cli/bin/webm_encoder
I0924 21:32:08.459241       1 analytics.go:187] CLI startup completed (took 285.555917ms)
W0924 21:32:08.461411     204 cache.go:135] Cache(loadCodeAssistResponse): Singleflight refresh failed: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.461511     204 errorreport.go:224] error getting token source: You are not logged into Antigravity.
W0924 21:32:08.461629     204 cache.go:135] Cache(userInfo): Singleflight refresh failed: failed to get load code assist response: error getting token source: You are not logged into Antigravity.
E0924 21:32:08.461662     204 errorreport.go:224] failed to get load code assist response: error getting token source: You are not logged into Antigravity.
I0924 21:32:08.478694     264 keyring.go:64] keyringAuth: loaded token, expiry=2026-09-24 22:02:58.773153 +0900 KST expired=false
I0924 21:32:08.666372     263 auth.go:157] ChainedAuth: authenticated via keyring (effective: keyring)
I0924 21:32:08.666556     263 server_oauth.go:196] applyAuthResult: email=<email>, authMethod=consumer, quotaProject=
I0924 21:32:08.666624     263 server_oauth.go:201] OAuth: authenticated successfully as <email>
I0924 21:32:08.666650     263 server_oauth.go:207] b.codeAssistClient.AuthProvider (0x2ea62b5ca0f0) is same as b.cliAuth (0x2ea62b5ca0f0)
W0924 21:32:08.667313      62 cache.go:163] Failed to refresh cache in background: admin controls not applicable
W0924 21:32:08.667432     340 cache.go:163] Failed to refresh cache in background: admin controls not applicable
I0924 21:32:11.027320     263 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:11.027444     263 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:11.027734     215 experiment_manager.go:66] Starting experiment refresh after login
I0924 21:32:11.027741     214 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 21:32:11.355480     215 remote_agent.go:156] Remote agent fastpush pin gate changed: false -> true
I0924 21:32:11.355712     215 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 21:32:11.355726     215 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 21:32:11.355735     215 experiment_manager.go:70] Experiments refreshed after login
I0924 21:32:11.355788     321 manager.go:1308] Reloading system slash commands
I0924 21:32:11.355776     337 conversation_manager.go:512] Starting new conversation (agent=false)
I0924 21:32:11.355858     337 server.go:1204] Creating new cascade trajectory (agentScript=false)
I0924 21:32:11.355870     337 server.go:1207] Conversation using project ID: default-cli-project
I0924 21:32:11.361291     337 server.go:1239] Created conversation 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.361349     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362358     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362421     337 conversation_manager.go:559] project: switching to conversation belonging to project ID: default-cli-project
I0924 21:32:11.362516     337 server.go:2211] Backend project ID updated dynamically to: default-cli-project
I0924 21:32:11.362528     337 cli_setting_manager.go:218] ApplyProjectPermissionGrants: no grants for project "CLI Project", cleared project permissions
I0924 21:32:11.362540     337 conversation_manager.go:605] project: synced active project to "CLI Project" (id=default-cli-project) from conversation switch
I0924 21:32:11.362551     337 conversation_manager.go:887] Streaming conversation 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.362602     337 server.go:3211] GetConversationDetail: found conversation 087c73bd-a900-4e13-9231-183e7a901038 (active=true)
I0924 21:32:11.362782     423 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:11.362834     423 manager.go:1308] Reloading system slash commands
I0924 21:32:11.363063     337 server.go:1248] Starting conversation update stream for 087c73bd-a900-4e13-9231-183e7a901038
I0924 21:32:11.363493     424 manager.go:538] Ignoring IDLE update because we are waiting for RUNNING
I0924 21:32:11.368788     423 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:14.526713     352 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:14.526830     352 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:14.527039      76 model_resolver.go:93] Resolving model gemini-3.8-flash-high
I0924 21:32:14.527148      76 model_config_manager.go:327] Propagating selected model override to backend: label="Gemini 3.8 Flash (High)"
I0924 21:32:14.527201     215 experiment_manager.go:66] Starting experiment refresh after login
I0924 21:32:14.527908     214 quota_manager.go:45] doRefreshQuota: starting reload (force=true)
I0924 21:32:14.538154     337 conversation_manager.go:699] Forwarding user message to conversation 087c73bd-a900-4e13-9231-183e7a901038 (items=1, media=0)
I0924 21:32:14.538352     337 server.go:1846] Sending user message to conversation 087c73bd-a900-4e13-9231-183e7a901038 (items=1, media=0)
I0924 21:32:14.562896     466 monitor_config_manager_external.go:53] externalMonitorConfigManager: initialized with embedded config (2 monitors)
I0924 21:32:14.563249     466 monitoring.go:318] [Sonar] Resolved active tags: [policy_guardian] (detected surface: "")
I0924 21:32:14.563357     466 monitoring.go:318] [Sonar] Resolved active tags: [policy_guardian] (detected surface: "")
W0924 21:32:14.563381     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "mcp_servers"
W0924 21:32:14.563439     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "user_rules"
W0924 21:32:14.563451     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "subagent_reminder"
W0924 21:32:14.563462     466 declarative_config_loader.go:272] skipping component during resolution: empty component: prompt section "terminal_sandbox"
W0924 21:32:14.563491     466 declarative_config_loader.go:272] skipping component during resolution: empty component: pre-tool hook "command_assessor" is empty
W0924 21:32:14.563508     466 declarative_config_loader.go:272] skipping component during resolution: empty component: post-tool hook "command_assessor" is empty
I0924 21:32:14.738775     215 server.go:3769] [RemoteControl] Session toggle is off, staying disconnected
I0924 21:32:14.738929     215 server.go:3480] [RemoteControl] Resolved proxyServerURL: ""
I0924 21:32:14.738961     215 experiment_manager.go:70] Experiments refreshed after login
I0924 21:32:14.739083     654 manager.go:1308] Reloading system slash commands
I0924 21:32:14.748952     654 manager.go:1312] Slash commands unchanged, skipping update
I0924 21:32:18.266684     214 quota_manager.go:41] doRefreshQuota: skipped (throttled)
I0924 21:32:19.637013     623 manager.go:1331] Reloading system slash commands and skills
I0924 21:32:19.637228     623 manager.go:1308] Reloading system slash commands
I0924 21:32:19.637963     616 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 21:32:19.644263     623 manager.go:1312] Slash commands unchanged, skipping update
"#;
    const REAL_MACOS_STARTUP_LINE: usize = 11;
    const REAL_MACOS_CONVERSATION_RELOAD_LINE: usize = 44;
    const REAL_MACOS_DEFERRED_RELOAD_LINE: usize = 72;

    // "Deferred" skills reload latency (`Reloading system slash commands and skills`
    // stamped at least 1 s after `CLI startup completed`, measured from the startup
    // line) in the fixtures above, all Agy 1.2.10 on the Windows machine, 2026-09-24
    // KST. Read with the 2026-10-01 finding: this is the trust reload, logged when the
    // paste's Enter approved the workspace-trust dialog, so it trails every lost paste
    // and is absent wherever the workspace was already trusted (the delivered rows).
    //
    // | fixture        | startup reload  | deferred reload      | latency  | paste                 |
    // |----------------|-----------------|----------------------|----------|-----------------------|
    // | session-udT6uY | +0.5 ms         | none before the paste| -        | +26.1 s, delivered    |
    // | session-fMqSQc | before startup  | 16:41:20.813553      | 12.983 s | +12 s, lost           |
    // | session-IQHEwf | before startup  | none in 5 minutes    | -        | none (round-1 gate)   |
    // | session-IEKjtC | before startup  | 17:47:13.468286      | 9.848 s  | +9.5 s, lost          |
    // | session-ql5TVc | before startup  | 18:48:25.481988      | 21.367 s | +20.1 s, lost         |
    // | session-M8QFPp | +1.6 ms         | none before the paste| -        | +8.5 s, delivered     |
    // | session-uqraap | before startup  | 20:44:37.855815      | 36.413 s | +35 s, lost           |
    //
    // session-Cf1FBY (20:36, heavy CPU load, not a fixture) logged its deferred
    // reload at +36.4 s as well and lost the 35 s window's paste the same way; the
    // observed latencies are 9.8, 13.0, 21.4, 36.4 and 36.4 s, a second cluster near
    // 36 s under load and idle alike. session-8WjG3m (not a fixture) logged
    // its receipt 49 s after startup, about 10 s after the paste at the window's
    // end, and was delivered; session-M8QFPp logged its receipt 18.0 s after the
    // paste, outside the former 15 s receipt window.
    //
    // The udT6uY `... and skills` line at 16:42:50.228104 (+26.15 s) followed the
    // receipt and `Starting new conversation` by 15 ms, so it is the conversation
    // reload, not the deferred one. Every delivered session shows this pattern (the
    // two round-7 live asks included): a skills reload logged right after
    // `HandleUserInput` and the conversation-start lines is triggered by the new
    // conversation and is never evidence that the paste was cleared; the receipt
    // check returns on the first read holding the receipt and ignores later lines
    // (`receipt_stays_delivered_when_the_conversation_reload_follows_it`).
    // The other Agy 1.2.10 logs on this machine that
    // are not fixtures (2026-09-24, read once for this table) logged the deferred
    // reload at 12.933, 12.942, 12.947, 12.949, 12.964, 13.082, 13.124, 13.137 and
    // 13.161 s (nine sessions, none pasted before it), and seven sessions whose
    // startup reload preceded startup delivered a paste at +25.7 to +34.1 s with no
    // deferred reload before the receipt. Ten sessions whose startup reload followed
    // `CLI startup completed` never logged a separate deferred reload before their
    // receipt (+9.6 to +27.6 s).

    // Round 3's rule, for contrast: the same quiet period without the deferred-reload
    // condition (a zero window is satisfied as soon as startup is seen).
    const ROUND_3_TIMING: ReadinessTiming = ReadinessTiming {
        quiet_period: STARTUP_QUIET_PERIOD,
        deferred_reload_window: Duration::ZERO,
        redraw_required: true,
    };
    // Round 6's rule, for contrast: the same quiet period with the former 20 s
    // deferred-reload window, which session-ql5TVc's 21.4 s reload outlasted.
    const ROUND_6_TIMING: ReadinessTiming = ReadinessTiming {
        quiet_period: STARTUP_QUIET_PERIOD,
        deferred_reload_window: Duration::from_secs(20),
        redraw_required: true,
    };
    // Round 8's rule, for contrast: the same quiet period with the former 35 s
    // deferred-reload window, which the 36.4 s reloads of session-uqraap and
    // session-Cf1FBY outlasted.
    const ROUND_8_TIMING: ReadinessTiming = ReadinessTiming {
        quiet_period: STARTUP_QUIET_PERIOD,
        deferred_reload_window: Duration::from_secs(35),
        redraw_required: true,
    };
    // The Windows console rule of 0.0.7 and 0.0.8, for contrast: the same quiet period
    // with the 45 s deferred-reload window, which no paste ever needed. A workspace
    // trusted before launch never logs the trust reload, so its paste waited the
    // whole window (session-jkxi48, 2026-10-01), and in an untrusted one the reload
    // follows the paste, whatever the window is.
    const DEFERRED_RELOAD_WINDOW: Duration = Duration::from_secs(45);
    const ROUND_9_TIMING: ReadinessTiming = ReadinessTiming {
        quiet_period: STARTUP_QUIET_PERIOD,
        deferred_reload_window: DEFERRED_RELOAD_WINDOW,
        redraw_required: true,
    };
    // Round 8's receipt window, which session-M8QFPp's 18.0 s receipt outlasted.
    const ROUND_8_RECEIPT_WINDOW: Duration = Duration::from_secs(15);
    const REPLAY_POLL: Duration = Duration::from_millis(100);

    // The glog stamp of a line, on the adapter's own parser, so the replay and the
    // deferred-reload rule read the same instants.
    fn glog_time_of_day(line: &str) -> Option<Duration> {
        glog_timestamp(line)
    }

    // Replays a recorded log against the fake clock: each complete line becomes
    // visible at its own glog timestamp, with the first timestamped line at `start`,
    // so the readiness instants the tests assert are the ones Agy recorded. A line
    // without a timestamp (`CLI ready for user input`, or a header before the first
    // timestamped line) and a line stamped earlier than its predecessor (threads log
    // out of order by a few milliseconds) appear together with the line before them;
    // untimestamped lines before the first timestamp appear at `start`.
    struct LogReplay {
        start: Instant,
        first: Duration,
        // The `mmdd` of the first timestamped line; a recorded log spans one day.
        date: String,
        lines: Vec<(Instant, String)>,
    }

    impl LogReplay {
        fn new(text: &str, start: Instant) -> Self {
            let (first, date) = text
                .lines()
                .find_map(|line| Some((glog_time_of_day(line)?, line[1..5].to_owned())))
                .expect("a recorded log has a timestamped line");
            let mut previous = first;
            let mut lines = Vec::new();
            for line in text.lines() {
                let stamp = glog_time_of_day(line).unwrap_or(previous).max(previous);
                previous = stamp;
                lines.push((start + (stamp - first), format!("{line}\n")));
            }
            Self {
                start,
                first,
                date,
                lines,
            }
        }

        // The replay instant of a recorded `HH:MM:SS.ffffff` time of day.
        fn recorded(&self, time: &str) -> Instant {
            let stamp = glog_time_of_day(&format!("I{} {time}", self.date)).expect("a glog time");
            self.start + (stamp - self.first)
        }

        fn visible_at(&self, now: Instant) -> Option<Vec<u8>> {
            let visible: String = self
                .lines
                .iter()
                .take_while(|(at, _)| *at <= now)
                .map(|(_, line)| line.as_str())
                .collect();
            (!visible.is_empty()).then(|| visible.into_bytes())
        }

        fn reader(&self, now: Rc<Cell<Instant>>) -> impl FnMut() -> Result<Option<Vec<u8>>> + '_ {
            move || Ok(self.visible_at(now.get()))
        }

        // The state of a fresh gate stepped through the recorded instants in order.
        fn states(&self, timing: ReadinessTiming, times: &[&str]) -> Vec<ReadinessState> {
            let mut gate = ReadinessGate::new(self.start, timing);
            times
                .iter()
                .map(|time| {
                    let now = self.recorded(time);
                    gate.observe(self.visible_at(now).as_deref(), now)
                })
                .collect()
        }
    }

    // Runs the gate over the replayed log, polling every 100 ms from the first line,
    // and returns the instant at which it reported ready.
    fn replay_readiness(replay: &LogReplay, timing: ReadinessTiming) -> Instant {
        let mut clock = FakeClock::new(replay.start);
        wait_for_startup_readiness_with(
            &mut replay.reader(clock.shared()),
            replay.start + Duration::from_secs(300),
            timing,
            REPLAY_POLL,
            &mut clock,
        )
        .expect("the replayed log passes the gate before the deadline");
        clock.now()
    }

    // The gate polls every 100 ms, so it reports ready at the first poll at or after
    // the exact instant.
    fn assert_ready_at(replay: &LogReplay, ready: Instant, expected: &str, what: &str) {
        let expected_at = replay.recorded(expected);
        assert!(
            ready >= expected_at && ready < expected_at + REPLAY_POLL,
            "{what}: ready {:?} after the first line, expected {expected} ({:?} after the first line)",
            ready.saturating_duration_since(replay.start),
            expected_at.saturating_duration_since(replay.start)
        );
    }

    fn glog(time: &str, thread: u32, source: &str, message: &str) -> String {
        format!("I0924 {time} {thread:>7} {source}] {message}\n")
    }

    const HOOKS_LOADED: &str = "loaded 0 named hooks from 0 hooks.json file(s)";
    const SKILLS_RELOAD: &str = "Reloading system slash commands and skills";
    const SLASH_RELOAD: &str = "Reloading system slash commands";
    const FULL_REDRAW: &str =
        "Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)";

    fn successful_startup_log() -> String {
        REAL_SUCCESS_STARTUP.to_owned()
    }

    fn late_reload_startup_log() -> String {
        REAL_FAILURE_STARTUP.to_owned()
    }

    fn late_reload_completion() -> String {
        REAL_FAILURE_LATE_RELOAD.to_owned()
    }

    // session-fMqSQc with its deferred reload logged: the deferred-reload condition
    // is settled, so the gate is ready one quiet period after the newest activity
    // line (the reload's hooks line). The tests about the quiet period, continuity,
    // and the paste offset use this shape, which is ready long before the window.
    fn settled_startup_log() -> String {
        late_reload_startup_log() + &late_reload_completion()
    }

    // The bytes of `log` before its `Full redraw completed` line: startup observed,
    // the redraw not yet.
    fn before_redraw(log: &str) -> &str {
        &log[..log.find(FULL_REDRAW_MARKER).unwrap()]
    }

    // Go `%q` formatting as observed in the HandleUserInput lines.
    fn go_quoted(text: &str) -> String {
        let mut quoted = String::from("\"");
        for character in text.chars() {
            match character {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                _ => quoted.push(character),
            }
        }
        quoted.push('"');
        quoted
    }

    fn receipt_line(quoted: &str) -> String {
        format!(
            "I0924 16:42:50.212542     595 input_loop.go:107] HandleUserInput called with text: {quoted}\n"
        )
    }

    fn framed_receipt_line(prompt: &str, pending: &PendingAgyTurn) -> String {
        let framed = terminal_correlated_prompt(prompt, pending, true).unwrap();
        receipt_line(&go_quoted(&framed))
    }

    fn long_markdown_prompt() -> String {
        let mut prompt = String::from(
            "[Agent Bridge native delegation]\nSource: external\n\nYou are the prose writer for a documentation task. The author has already decided everything about the content. Your job is expression only.\n\n## Input\n\n- `brief.md` in this directory is the Content Brief.\n",
        );
        while prompt.len() < 1600 {
            prompt.push_str(
                "- Keep every claim from the brief; do not add \"facts\" or C:\\paths.\n",
            );
        }
        prompt
    }

    struct FakeClock {
        now: Rc<Cell<Instant>>,
        slept: Duration,
    }

    impl FakeClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Rc::new(Cell::new(now)),
                slept: Duration::ZERO,
            }
        }

        // The clock's instant, for a log reader that grows the log with the clock.
        fn shared(&self) -> Rc<Cell<Instant>> {
            Rc::clone(&self.now)
        }
    }

    impl Clock for FakeClock {
        fn now(&mut self) -> Instant {
            self.now.get()
        }

        fn sleep(&mut self, duration: Duration) {
            self.now.set(self.now.get() + duration);
            self.slept += duration;
        }
    }

    fn log_sequence(logs: Vec<Result<Option<Vec<u8>>>>) -> impl FnMut() -> Result<Option<Vec<u8>>> {
        let mut logs = std::collections::VecDeque::from(logs);
        move || match logs.len() {
            0 => panic!("the log sequence was exhausted"),
            1 => match logs.front().unwrap() {
                Ok(log) => Ok(log.clone()),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            },
            _ => logs.pop_front().unwrap(),
        }
    }

    fn some_log(text: &str) -> Result<Option<Vec<u8>>> {
        Ok(Some(text.as_bytes().to_vec()))
    }

    #[test]
    fn startup_observation_tracks_the_markers_and_the_newest_activity_line() {
        // session-udT6uY: main-thread hooks line, startup, skills reload with its
        // hooks line, redraw, three plain reloads.
        let observation = observe_startup(successful_startup_log().as_bytes());
        assert_eq!(observation.startup_line, Some(3));
        assert_eq!(
            observation.startup_stamp,
            glog_timestamp("I0924 16:42:24.080467       1 analytics.go:187] CLI startup completed")
        );
        assert_eq!(observation.redraw_after_startup, Some(7));
        assert_eq!(
            observation.deferred_reload_after_startup, None,
            "the skills reload stamped 0.5 ms after startup is the startup reload, not the deferred one"
        );
        assert_eq!(observation.startup_reload_after_startup, Some(4));
        assert_eq!(observation.settle_line, Some(10));
        assert_eq!(observation.activity_lines, 8);
        assert!(observation.markers_observed(true));
        assert!(observation.missing_markers(true).is_empty());

        // session-fMqSQc: the startup reload precedes startup and has no hooks line;
        // the markers are still complete, the newest activity is the last reload, and
        // no skills reload has followed startup yet.
        let late = late_reload_startup_log();
        let observation = observe_startup(late.as_bytes());
        assert_eq!(observation.startup_line, Some(6));
        assert_eq!(observation.redraw_after_startup, Some(7));
        assert_eq!(observation.deferred_reload_after_startup, None);
        assert_eq!(observation.startup_reload_after_startup, None);
        assert_eq!(observation.settle_line, Some(9));
        assert_eq!(observation.activity_lines, 6);
        assert!(observation.markers_observed(true));

        // The late reload (13.0 s after startup) is the deferred skills reload; it and
        // its hooks line are also activity.
        let completed = late + &late_reload_completion();
        let observation = observe_startup(completed.as_bytes());
        assert_eq!(observation.startup_line, Some(6));
        assert_eq!(observation.redraw_after_startup, Some(7));
        assert_eq!(observation.deferred_reload_after_startup, Some(11));
        assert_eq!(observation.startup_reload_after_startup, None);
        assert_eq!(observation.settle_line, Some(13));
        assert_eq!(observation.activity_lines, 9);

        // session-IQHEwf: the healthy startup that never logs the reload/hooks pair.
        let observation = observe_startup(REAL_QUIET_STARTUP.as_bytes());
        assert_eq!(observation.startup_line, Some(15));
        assert_eq!(observation.redraw_after_startup, Some(16));
        assert_eq!(observation.deferred_reload_after_startup, None);
        assert_eq!(observation.settle_line, Some(47));
        assert_eq!(observation.activity_lines, 6);
        assert!(observation.markers_observed(true));

        // session-IEKjtC: the startup reload precedes startup; the deferred skills
        // reload and its hooks line come 9.8 s later.
        let observation = observe_startup(REAL_DEFERRED_RELOAD_STARTUP.as_bytes());
        assert_eq!(observation.startup_line, Some(12));
        assert_eq!(observation.redraw_after_startup, Some(13));
        assert_eq!(observation.deferred_reload_after_startup, Some(46));
        assert_eq!(observation.settle_line, Some(48));
        assert_eq!(observation.activity_lines, 9);
        assert!(observation.markers_observed(true));

        // session-M8QFPp: the startup reload follows startup by 1.6 ms with its hooks
        // line; it is not the deferred reload, and no deferred reload follows.
        let observation = observe_startup(REAL_STARTUP_RELOAD_AFTER_STARTUP.as_bytes());
        assert_eq!(observation.startup_line, Some(0));
        assert_eq!(observation.redraw_after_startup, Some(4));
        assert_eq!(observation.deferred_reload_after_startup, None);
        assert_eq!(observation.startup_reload_after_startup, Some(1));
        assert_eq!(observation.settle_line, Some(33));
        assert_eq!(observation.activity_lines, 7);
        assert!(observation.markers_observed(true));

        // session-uqraap: the startup reload precedes startup; the deferred skills
        // reload and its hooks line come 36.4 s later.
        let observation = observe_startup(REAL_SECOND_CLUSTER_RELOAD_STARTUP.as_bytes());
        assert_eq!(observation.startup_line, Some(22));
        assert_eq!(observation.redraw_after_startup, Some(23));
        assert_eq!(observation.deferred_reload_after_startup, Some(50));
        assert_eq!(observation.startup_reload_after_startup, None);
        assert_eq!(observation.settle_line, Some(52));
        assert_eq!(observation.activity_lines, 8);
        assert!(observation.markers_observed(true));

        // The 1 s rule on synthetic stamps: 999 ms after startup is the startup
        // reload, 1.000 s is the deferred one, a reload stamped a few milliseconds
        // before startup by another thread is the startup reload, and a reload
        // without a stamp proves nothing.
        let startup = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        );
        for (stamp, deferred) in [
            ("16:41:07.826763", false),
            ("16:41:08.830344", false),
            ("16:41:08.830345", true),
            ("16:41:20.813553", true),
        ] {
            let log = startup.clone() + &glog(stamp, 410, "manager.go:1331", SKILLS_RELOAD);
            let observation = observe_startup(log.as_bytes());
            assert_eq!(
                observation.deferred_reload_after_startup.is_some(),
                deferred,
                "a skills reload stamped {stamp} after startup at 16:41:07.830345"
            );
            assert_eq!(
                observation.startup_reload_after_startup.is_some(),
                !deferred
            );
        }
        let unstamped = startup.clone() + "manager.go:1331] " + SKILLS_RELOAD + "\n";
        assert_eq!(
            observe_startup(unstamped.as_bytes()).deferred_reload_after_startup,
            None
        );
        let startup_unstamped = "CLI startup completed (took 1ms)\n".to_owned()
            + &glog("16:41:20.813553", 410, "manager.go:1331", SKILLS_RELOAD);
        let observation = observe_startup(startup_unstamped.as_bytes());
        assert_eq!(observation.startup_stamp, None);
        assert_eq!(observation.deferred_reload_after_startup, None);
        // A stamp across midnight still measures the latency.
        let midnight = "I0924 23:59:59.500000       1 analytics.go:187] CLI startup completed\n"
            .to_owned()
            + "I0925 00:00:12.500000     410 manager.go:1331] Reloading system slash commands and skills\n";
        assert_eq!(
            observe_startup(midnight.as_bytes()).deferred_reload_after_startup,
            Some(1)
        );
        assert_eq!(
            glog_timestamp("F0924 16:42:24.080467       1 x.go:1] fatal").map(|stamp| stamp
                - glog_timestamp("I0924 16:42:24.080467       1 x.go:1] info").unwrap()),
            Some(Duration::ZERO)
        );
        assert_eq!(glog_timestamp("CLI ready for user input"), None);
        assert_eq!(
            glog_timestamp("I0924 16:42:24.08046        1 x.go:1] short"),
            None
        );

        // A redraw before startup does not satisfy the second marker.
        let redraw_first = glog("16:41:07.000000", 269, "manager.go:934", FULL_REDRAW)
            + &glog(
                "16:41:07.830345",
                1,
                "analytics.go:187",
                "CLI startup completed (took 1ms)",
            );
        let observation = observe_startup(redraw_first.as_bytes());
        assert_eq!(observation.startup_line, Some(1));
        assert_eq!(observation.redraw_after_startup, None);
        assert_eq!(observation.settle_line, Some(0));
        assert_eq!(
            observation.missing_markers(true),
            vec![REDRAW_AFTER_STARTUP_DESCRIPTION]
        );

        let no_startup = successful_startup_log().replace("CLI startup completed", "CLI startup");
        let observation = observe_startup(no_startup.as_bytes());
        assert!(!observation.startup_completed());
        assert_eq!(observation.redraw_after_startup, None);
        assert_eq!(
            StartupObservation::default().missing_markers(true),
            vec!["`CLI startup completed`", REDRAW_AFTER_STARTUP_DESCRIPTION]
        );
    }

    #[test]
    fn startup_readiness_ignores_partial_writes_and_terminal_noise() {
        let mut partial = late_reload_startup_log();
        partial.push_str(
            "I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills\nI0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named ho",
        );
        let observation = observe_startup(partial.as_bytes());
        assert_eq!(observation.settle_line, Some(11));
        assert_eq!(observation.activity_lines, 7);
        partial.push_str("oks from 0 hooks.json file(s)\n");
        let observation = observe_startup(partial.as_bytes());
        assert_eq!(
            observation.settle_line,
            Some(12),
            "a hooks line restarts the quiet period once it is complete"
        );
        assert_eq!(observation.activity_lines, 8);

        let mut partial_startup = REAL_QUIET_STARTUP
            [..REAL_QUIET_STARTUP.find("I0924 17:20:28.663151").unwrap()]
            .to_owned();
        partial_startup.push_str("I0924 17:20:28.663151     342 manager.go:934] Full redraw comp");
        assert_eq!(
            observe_startup(partial_startup.as_bytes()).redraw_after_startup,
            None
        );

        let noisy = successful_startup_log()
            .lines()
            .map(|line| format!("\u{1b}[32m{line}\u{1b}[0m\r\n"))
            .collect::<String>()
            .replace("CLI startup", "CLI\u{1b}]0;title\u{7} startup")
            .replace(
                "redraw completed",
                "redraw\u{1b}]8;;file:///x\u{1b}\\ completed",
            );
        let observation = observe_startup(noisy.as_bytes());
        assert!(observation.startup_completed());
        assert_eq!(observation.redraw_after_startup, Some(7));
        assert_eq!(observation.activity_lines, 8);
        assert_eq!(strip_terminal_noise("a\u{1b}[1;31mb\u{1b}Kc\r"), "abc");
        assert_eq!(
            strip_terminal_noise("\u{1b}]0;title\u{1b}\\CLI startup completed"),
            "CLI startup completed",
            "an OSC sequence ends at the string terminator ESC backslash"
        );
        assert_eq!(strip_terminal_noise("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(
            strip_terminal_noise("a\u{1b}]0;title\u{1b}[0mb"),
            "ab",
            "another escape ends an unterminated OSC and is stripped on its own"
        );
        assert_eq!(strip_terminal_noise("a\u{1b}]0;unterminated"), "a");
    }

    #[test]
    fn input_receipt_is_recognised_after_an_st_terminated_osc_sequence() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let receipt = framed_receipt_line("hello", &pending);
        let decorated = format!("\u{1b}]0;agy\u{1b}\\{receipt}");
        assert_eq!(
            observe_input_receipt(Some(decorated.as_bytes()), 0, &pending),
            ReceiptEvidence::Delivered
        );
        let bel = format!("\u{1b}]0;agy\u{7}{receipt}");
        assert_eq!(
            observe_input_receipt(Some(bel.as_bytes()), 0, &pending),
            ReceiptEvidence::Delivered
        );
    }

    #[test]
    fn readiness_gate_is_ready_after_the_quiet_period_on_the_quiet_healthy_startup() {
        // session-IQHEwf: the round-1/2 rule required a hooks_manager.go line after
        // the latest `... and skills` reload. This log has none, so that rule would
        // have waited until the deadline; the receipt-less silence was a healthy
        // idle composer.
        let lines: Vec<String> = complete_log_lines(REAL_QUIET_STARTUP.as_bytes()).collect();
        let latest_skills_reload = lines
            .iter()
            .rposition(|line| line.contains(SKILLS_RELOAD_MARKER))
            .unwrap();
        assert!(
            lines[latest_skills_reload..]
                .iter()
                .all(|line| !line.contains(HOOKS_LOADED_SOURCE)),
            "the old readiness discriminator never appears in this healthy log"
        );

        // The trust reload never comes either: the workspace was trusted before
        // launch. The gate is ready at 17:20:38.45, 3.5 s after the last plain
        // reload and 9.8 s after `CLI startup completed` (17:20:28.616574).
        let replay = LogReplay::new(REAL_QUIET_STARTUP, Instant::now());
        let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, ready, "17:20:38.450075", "session-IQHEwf");
        assert_eq!(
            replay.states(
                WINDOWS_STARTUP_READINESS_TIMING,
                &[
                    "17:20:28.610124",
                    "17:20:28.616574",
                    "17:20:28.663151",
                    "17:20:30.481001",
                    "17:20:33.155749",
                    "17:20:34.950075",
                    "17:20:38.450074",
                    "17:20:38.450075",
                ]
            ),
            vec![
                ReadinessState::AwaitingStartup,
                ReadinessState::AwaitingRedraw,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Ready,
            ]
        );

        // The former rules waited for that reload all the same, each for its whole
        // window, counted from when the gate first saw startup: the 45 s rule pasted
        // at 17:21:13.62, the 35 s one at 17:21:03.62, and the quiet period that ran
        // concurrently had long ended. The window's end was the ready instant, not
        // window + quiet period.
        assert_ready_at(
            &replay,
            replay_readiness(&replay, ROUND_9_TIMING),
            "17:21:13.616574",
            "session-IQHEwf, the 45 s rule",
        );
        assert_ready_at(
            &replay,
            replay_readiness(&replay, ROUND_8_TIMING),
            "17:21:03.616574",
            "session-IQHEwf, round 8",
        );
        assert_eq!(
            replay_readiness(&replay, ROUND_3_TIMING),
            ready,
            "round 3's rule is the rule again, now behind the trust evidence"
        );

        // State by state under the 45 s rule: every plain reload restarts the quiet
        // period, and the deferred-reload window is the last condition to hold.
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "17:20:28.610124",
                    "17:20:28.616574",
                    "17:20:28.663151",
                    "17:20:30.481001",
                    "17:20:33.155749",
                    "17:20:34.950075",
                    "17:20:38.450075",
                    "17:20:48.616574",
                    "17:21:03.616574",
                    "17:21:13.616573",
                    "17:21:13.616574",
                ]
            ),
            vec![
                ReadinessState::AwaitingStartup,
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Ready,
            ]
        );
        let mut gate = ReadinessGate::new(replay.start, WINDOWS_STARTUP_READINESS_TIMING);
        assert_eq!(
            gate.observe(None, replay.start),
            ReadinessState::AwaitingLog
        );
    }

    #[test]
    fn readiness_gate_starts_the_quiet_period_at_the_newest_activity_line() {
        let start = Instant::now();
        // A rule with the former 45 s window, so that the quiet period is exercised
        // together with the condition it once ran concurrently with.
        let timing = ReadinessTiming {
            quiet_period: Duration::from_millis(3500),
            deferred_reload_window: Duration::from_secs(45),
            redraw_required: true,
        };
        let at = |millis: u64| start + Duration::from_millis(millis);

        // session-fMqSQc with its deferred reload logged: markers complete and the
        // deferred-reload condition settled; ready after one quiet period.
        let mut gate = ReadinessGate::new(start, timing);
        let success = settled_startup_log();
        assert_eq!(
            gate.observe(Some(success.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(success.as_bytes()), at(3_400)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(success.as_bytes()), at(3_500)),
            ReadinessState::Ready
        );

        // Any activity line restarts the quiet period: a plain reload, a skills
        // reload, a hooks line, or a redraw.
        let mut restarted = success.clone();
        for (millis, line) in [
            (
                3_600,
                glog("16:42:29.500000", 600, "manager.go:1308", SLASH_RELOAD),
            ),
            (
                7_200,
                glog("16:42:33.100000", 601, "manager.go:1331", SKILLS_RELOAD),
            ),
            (
                10_800,
                glog("16:42:36.700000", 602, "hooks_manager.go:53", HOOKS_LOADED),
            ),
            (
                14_400,
                glog("16:42:40.300000", 603, "manager.go:934", FULL_REDRAW),
            ),
        ] {
            restarted.push_str(&line);
            assert_eq!(
                gate.observe(Some(restarted.as_bytes()), at(millis)),
                ReadinessState::Settling,
                "{line:?} restarts the quiet period"
            );
            assert_eq!(
                gate.observe(Some(restarted.as_bytes()), at(millis + 3_499)),
                ReadinessState::Settling
            );
            assert_eq!(
                gate.observe(Some(restarted.as_bytes()), at(millis + 3_500)),
                ReadinessState::Ready
            );
        }

        // The quiet period counts from when the gate first saw the newest activity
        // line, so a log that is already old when the gate starts is ready after one
        // quiet period, not immediately.
        let mut aged = ReadinessGate::new(start, timing);
        assert_eq!(
            aged.observe(Some(success.as_bytes()), at(60_000)),
            ReadinessState::Settling
        );
        assert_eq!(
            aged.observe(Some(success.as_bytes()), at(63_500)),
            ReadinessState::Ready
        );

        // Startup without a redraw after it is not ready however quiet the log is.
        let mut no_redraw = ReadinessGate::new(start, timing);
        let startup_only = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        );
        assert_eq!(
            no_redraw.observe(Some(startup_only.as_bytes()), at(0)),
            ReadinessState::AwaitingRedraw
        );
        assert_eq!(
            no_redraw.observe(Some(startup_only.as_bytes()), at(55_000)),
            ReadinessState::AwaitingRedraw
        );
        let redrawn =
            startup_only.clone() + &glog("16:42:02.000000", 269, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            no_redraw.observe(Some(redrawn.as_bytes()), at(55_100)),
            ReadinessState::Settling,
            "the deferred-reload window ended 10 s ago, counted from when startup was first seen"
        );
        assert_eq!(
            no_redraw.observe(Some(redrawn.as_bytes()), at(58_600)),
            ReadinessState::Ready
        );

        // Startup and redraw without a skills reload after them wait for the window
        // even though the quiet period ends at 3.5 s; the two run concurrently, so
        // the window's end at 45.0 s is the ready instant.
        let startup_redraw =
            startup_only.clone() + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
        let mut windowed = ReadinessGate::new(start, timing);
        assert_eq!(
            windowed.observe(Some(startup_redraw.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            windowed.observe(Some(startup_redraw.as_bytes()), at(44_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            windowed.observe(Some(startup_redraw.as_bytes()), at(45_000)),
            ReadinessState::Ready
        );

        // A skills reload stamped within 1 s of startup is the startup reload: it
        // restarts the quiet period but never settles the deferred-reload condition
        // (session-udT6uY at +0.5 ms, session-M8QFPp at +1.6 ms).
        let mut early = ReadinessGate::new(start, timing);
        let startup_reload_after = startup_only.clone()
            + &glog("16:41:07.831997", 338, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:41:07.831997", 338, "manager.go:1308", SLASH_RELOAD)
            + &glog("16:41:07.833248", 96, "hooks_manager.go:53", HOOKS_LOADED)
            + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            early.observe(Some(startup_reload_after.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            early.observe(Some(startup_reload_after.as_bytes()), at(44_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            early.observe(Some(startup_reload_after.as_bytes()), at(45_000)),
            ReadinessState::Ready
        );

        // A skills reload after startup ends the window at once; its own quiet period
        // follows, and a hooks line after it is never required.
        let mut reloaded = ReadinessGate::new(start, timing);
        assert_eq!(
            reloaded.observe(Some(startup_redraw.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        let with_reload = startup_redraw.clone()
            + &glog("16:41:12.000000", 410, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:41:12.000000", 410, "manager.go:1308", SLASH_RELOAD);
        assert_eq!(
            reloaded.observe(Some(with_reload.as_bytes()), at(4_200)),
            ReadinessState::Settling
        );
        assert_eq!(
            reloaded.observe(Some(with_reload.as_bytes()), at(7_699)),
            ReadinessState::Settling
        );
        assert_eq!(
            reloaded.observe(Some(with_reload.as_bytes()), at(7_700)),
            ReadinessState::Ready
        );

        // A skills reload before `CLI startup completed` is the startup reload, not
        // the deferred one.
        let mut startup_reload = ReadinessGate::new(start, timing);
        let reload_first = glog("16:41:07.826763", 280, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:41:07.826763", 280, "manager.go:1308", SLASH_RELOAD)
            + &startup_redraw;
        assert_eq!(
            startup_reload.observe(Some(reload_first.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            startup_reload.observe(Some(reload_first.as_bytes()), at(44_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            startup_reload.observe(Some(reload_first.as_bytes()), at(45_000)),
            ReadinessState::Ready
        );

        let mut no_startup = ReadinessGate::new(start, timing);
        let redraw_only = glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            no_startup.observe(Some(redraw_only.as_bytes()), at(10_000)),
            ReadinessState::AwaitingStartup
        );
    }

    #[test]
    fn log_continuity_accepts_only_a_log_that_extends_the_observed_content() {
        let observed = successful_startup_log();
        let continuity = LogContinuity::of(observed.as_bytes());
        assert_eq!(continuity.len, observed.len());
        assert_eq!(continuity.digest, log_digest(observed.as_bytes()));

        // Growth, including a partial trailing line that later completes, continues.
        let grown = observed.clone() + "I0924 16:42:30.000000     540 manager.go:1308] Reloading";
        assert_eq!(continuity.discontinuity(grown.as_bytes()), None);
        assert_eq!(
            LogContinuity::of(grown.as_bytes())
                .discontinuity((grown.clone() + " system slash commands\n").as_bytes()),
            None
        );
        assert_eq!(continuity.discontinuity(observed.as_bytes()), None);

        // A shorter file, or one with different leading bytes, does not.
        assert_eq!(
            continuity.discontinuity(&observed.as_bytes()[..observed.len() - 1]),
            Some(LogDiscontinuity::Shrunk {
                from: observed.len(),
                to: observed.len() - 1
            })
        );
        let replaced = observed.replace("16:42:24", "16:52:24");
        assert_eq!(replaced.len(), observed.len());
        assert_eq!(
            continuity.discontinuity(replaced.as_bytes()),
            Some(LogDiscontinuity::Replaced {
                observed: observed.len()
            })
        );

        // Every observed byte is compared, not a leading window: in a log well beyond
        // 4 KiB, a one-byte change in a late line (the round-5 counterexample, which
        // the 4 KiB prefix accepted as continuation), a rewrite of the last line that
        // keeps its index and the length, and a change in the first line are all
        // discontinuities.
        let long = REAL_QUIET_STARTUP.to_owned() + REAL_DEFERRED_RELOAD_STARTUP;
        assert!(long.len() > 4096);
        let continuity = LogContinuity::of(long.as_bytes());
        let changed_late = long.replace("17:47:13.468286", "17:47:13.468287");
        assert_ne!(changed_late, long);
        assert_eq!(changed_late.len(), long.len());
        assert_eq!(
            continuity.discontinuity(changed_late.as_bytes()),
            Some(LogDiscontinuity::Replaced {
                observed: long.len()
            }),
            "a rewrite beyond 4 KiB is a discontinuity"
        );
        let last_line = long.lines().last().unwrap();
        let rewritten_tail = long[..long.len() - last_line.len() - 1].to_owned()
            + &last_line.chars().rev().collect::<String>()
            + "\n";
        assert_eq!(rewritten_tail.len(), long.len());
        assert_eq!(rewritten_tail.lines().count(), long.lines().count());
        assert_eq!(
            continuity.discontinuity(rewritten_tail.as_bytes()),
            Some(LogDiscontinuity::Replaced {
                observed: long.len()
            }),
            "a suffix rewrite that keeps every line index is a discontinuity"
        );
        assert_eq!(
            continuity.discontinuity((rewritten_tail + "more\n").as_bytes()),
            Some(LogDiscontinuity::Replaced {
                observed: long.len()
            }),
            "a longer log whose observed prefix was rewritten is a discontinuity"
        );
        let changed_early = long.replacen("17:20:28.610124", "17:20:28.610125", 1);
        assert_eq!(
            continuity.discontinuity(changed_early.as_bytes()),
            Some(LogDiscontinuity::Replaced {
                observed: long.len()
            })
        );
        assert_eq!(
            continuity.discontinuity((long.clone() + &changed_late).as_bytes()),
            None,
            "only the observed prefix is digested; appended bytes are free"
        );
    }

    #[test]
    fn readiness_gate_restarts_its_evidence_when_the_log_is_replaced() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        // The 45 s rule, under which the window has to restart as well as the quiet
        // period. The rule in force has no window; its quiet period restarts the
        // same way (the last block of this test).
        let timing = ROUND_9_TIMING;
        let startup_redraw = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        ) + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);

        // The review scenario: startup and redraw at t=0, the log missing at t=19.9,
        // and at t=20 a replacement with the same content at the same indices. The
        // round-4 gate kept `startup_seen_at` from t=0 and the settle line index
        // matched, so it was ready at once although the fresh redraw had no quiet
        // period. The evidence now restarts at the disappearance: the replacement
        // is a new startup, and the window and the quiet period count from t=20.
        let mut gate = ReadinessGate::new(start, timing);
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(gate.observe(None, at(19_900)), ReadinessState::AwaitingLog);
        assert_eq!(
            gate.discontinuities,
            vec![(
                Duration::from_millis(19_900),
                LogDiscontinuity::Missing {
                    observed: startup_redraw.len()
                }
            )]
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(20_000)),
            ReadinessState::AwaitingDeferredReload,
            "a replacement with identical indices at t=20 is not ready"
        );
        assert_eq!(gate.startup_seen_at, Some(at(20_000)));
        assert_eq!(gate.settled_at, at(20_000));
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(64_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(65_000)),
            ReadinessState::Ready,
            "the replacement is ready when its own 45 s window ends"
        );
        assert_eq!(
            gate.discontinuities.len(),
            1,
            "the reappearance is not counted again"
        );

        // Replacement without an observed missing interval: the same lines at the
        // same indices and the same length, re-stamped by a fresh process. The
        // leading bytes differ, so the evidence restarts at t=20 as well.
        let restamped = startup_redraw.replace("16:41:07", "16:51:07");
        assert_eq!(restamped.len(), startup_redraw.len());
        let mut gate = ReadinessGate::new(start, timing);
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(0)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(19_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            gate.observe(Some(restamped.as_bytes()), at(20_000)),
            ReadinessState::AwaitingDeferredReload,
            "a replaced log with identical indices at t=20 is not ready"
        );
        assert_eq!(
            gate.discontinuities,
            vec![(
                Duration::from_millis(20_000),
                LogDiscontinuity::Replaced {
                    observed: startup_redraw.len()
                }
            )]
        );
        assert_eq!(
            gate.observe(Some(restamped.as_bytes()), at(64_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            gate.observe(Some(restamped.as_bytes()), at(65_000)),
            ReadinessState::Ready
        );

        // A replacement whose skills reload after startup would have satisfied the
        // deferred-reload condition still owes a full quiet period from the
        // observation of the new content, not from the old settle instant.
        let reloaded = startup_redraw.clone()
            + &glog("16:41:12.000000", 410, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:41:12.000000", 410, "manager.go:1308", SLASH_RELOAD);
        let mut gate = ReadinessGate::new(start, timing);
        assert_eq!(
            gate.observe(Some(reloaded.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(reloaded.as_bytes()), at(3_500)),
            ReadinessState::Ready
        );
        let fresh = reloaded.replace("16:41:", "16:51:");
        assert_eq!(
            gate.observe(Some(fresh.as_bytes()), at(3_600)),
            ReadinessState::Settling,
            "a fresh reload at the old index is not settled"
        );
        assert_eq!(
            gate.observe(Some(fresh.as_bytes()), at(7_099)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(fresh.as_bytes()), at(7_100)),
            ReadinessState::Ready
        );

        // Rotation to a shorter file: the tail of session-udT6uY without its startup
        // is a new log with no startup line, and a startup appended to it later is a
        // new startup.
        let observed = settled_startup_log();
        let tail: String = observed
            .lines()
            .skip(8)
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(tail.len() < observed.len());
        assert!(!tail.contains(STARTUP_COMPLETED_MARKER));
        let mut gate = ReadinessGate::new(start, timing);
        assert_eq!(
            gate.observe(Some(observed.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(tail.as_bytes()), at(5_000)),
            ReadinessState::AwaitingStartup
        );
        assert_eq!(
            gate.discontinuities,
            vec![(
                Duration::from_secs(5),
                LogDiscontinuity::Shrunk {
                    from: observed.len(),
                    to: tail.len()
                }
            )]
        );
        assert_eq!(gate.startup_seen_at, None);
        let rotated_startup = tail.clone()
            + &glog(
                "16:42:35.000000",
                1,
                "analytics.go:187",
                "CLI startup completed (took 1ms)",
            )
            + &glog("16:42:35.050000", 269, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            gate.observe(Some(rotated_startup.as_bytes()), at(6_000)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(gate.startup_seen_at, Some(at(6_000)));
        assert_eq!(
            gate.observe(Some(rotated_startup.as_bytes()), at(50_900)),
            ReadinessState::AwaitingDeferredReload
        );
        assert_eq!(
            gate.observe(Some(rotated_startup.as_bytes()), at(51_000)),
            ReadinessState::Ready
        );
        assert_eq!(gate.discontinuities.len(), 1);

        // A log that is missing before it was ever observed is not a discontinuity.
        let mut gate = ReadinessGate::new(start, timing);
        assert_eq!(gate.observe(None, at(0)), ReadinessState::AwaitingLog);
        assert_eq!(gate.observe(None, at(100)), ReadinessState::AwaitingLog);
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(200)),
            ReadinessState::AwaitingDeferredReload
        );
        assert!(gate.discontinuities.is_empty());

        // The rule in force, on the review scenario: ready one quiet period after
        // the redraw, and the replacement at t=20 owes a quiet period of its own.
        let mut gate = ReadinessGate::new(start, WINDOWS_STARTUP_READINESS_TIMING);
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(3_500)),
            ReadinessState::Ready
        );
        assert_eq!(gate.observe(None, at(19_900)), ReadinessState::AwaitingLog);
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(20_000)),
            ReadinessState::Settling,
            "a replacement with identical indices at t=20 is not ready"
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(23_499)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(startup_redraw.as_bytes()), at(23_500)),
            ReadinessState::Ready
        );
        assert_eq!(gate.discontinuities.len(), 1);
    }

    #[test]
    fn readiness_wait_restarts_after_every_log_discontinuity_within_the_deadline() {
        let start = Instant::now();
        let poll = Duration::from_millis(100);
        let deadline = start + Duration::from_secs(300);
        let observed = settled_startup_log();
        let half = before_redraw(&observed);
        assert!(half.contains(STARTUP_COMPLETED_MARKER));
        assert!(!half.contains(FULL_REDRAW_MARKER));

        // Two replacements without a missing interval: each restarts the evidence,
        // and the log observed after the second one is ready one quiet period after
        // it was first seen. The round-5 gate failed `not_sent` at 200 ms here. The
        // returned offset is the length of the read that passed.
        let restamped = observed.replace("16:41:", "16:51:");
        let restamped_again = observed.replace("16:41:", "17:01:");
        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![
                some_log(&observed),
                some_log(&restamped),
                some_log(&restamped_again),
            ]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, restamped_again.len());
        assert_eq!(
            clock.slept,
            Duration::from_millis(200) + STARTUP_QUIET_PERIOD
        );

        // Disappearance, reappearance, then rotation to a tail without the redraw:
        // the wait continues past the second discontinuity and fails only at the
        // deadline, naming both discontinuities in order and the missing marker.
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![
                some_log(&observed),
                Ok(None),
                some_log(&observed),
                some_log(half),
            ]),
            start + Duration::from_secs(2),
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Agy did not report startup readiness before the deadline"));
        assert!(message.contains(&format!(
            "the readiness evidence was restarted after 2 log discontinuities (agy.log disappeared after {} bytes had been observed at 100 ms; then agy.log shrank from {} to {} bytes (rotated or truncated) at 300 ms)",
            observed.len(),
            observed.len(),
            half.len()
        )));
        assert!(message.contains(&format!(
            "missing markers: {REDRAW_AFTER_STARTUP_DESCRIPTION}, "
        )));
        assert!(message.contains("the prompt was not pasted"));
        assert!(!message.contains("tolerated"));
        assert_eq!(clock.slept, Duration::from_secs(2));

        // Three discontinuities, then a stable log: still ready within the deadline.
        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![
                some_log(&observed),
                some_log(half),
                Ok(None),
                some_log(&restamped),
                some_log(&restamped_again),
            ]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, restamped_again.len());
        assert_eq!(
            clock.slept,
            Duration::from_millis(400) + STARTUP_QUIET_PERIOD
        );

        // A single replacement restarts the wait: the restamped log is ready one
        // quiet period after it was first observed, not at once.
        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![
                some_log(&observed),
                some_log(&observed),
                some_log(&observed),
                some_log(&restamped),
            ]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, restamped.len());
        assert_eq!(
            clock.slept,
            Duration::from_millis(300) + STARTUP_QUIET_PERIOD
        );

        // The deadline report names a single discontinuity that was tolerated.
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&observed), Ok(None), some_log(&observed)]),
            start + Duration::from_secs(2),
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Agy did not report startup readiness before the deadline"));
        assert!(message.contains(&format!(
            "the readiness evidence was restarted after 1 log discontinuity (agy.log disappeared after {} bytes had been observed at 100 ms)",
            observed.len()
        )));
        assert!(message.contains(&format!(
            "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period"
        )));
    }

    // The read that supplies the paste offset is a gate observation: when the read
    // at which the quiet period would end shows a fresh reload, a replacement, or no
    // log at all, nothing is pasted; the gate waits for a later read that passes
    // both the readiness rule and continuity and returns that read's length.
    #[test]
    fn readiness_wait_pastes_only_after_a_read_that_passes_readiness_and_continuity() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(300);
        let poll = Duration::from_millis(100);
        let startup = settled_startup_log();
        let fresh_reload = glog("16:42:32.000000", 540, "manager.go:1308", SLASH_RELOAD);
        let reloaded = startup.clone() + &fresh_reload;
        let restamped = startup.replace("16:41:", "16:51:");
        assert_eq!(restamped.len(), startup.len());
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        // The static startup log is ready when its quiet period ends.
        let ready_at = STARTUP_QUIET_PERIOD;
        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup)]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, startup.len());
        assert_eq!(clock.slept, ready_at);

        // A reader whose observation at `ready_at` is `at_ready`, and `after` from
        // then on; every earlier read is the startup log.
        let waited = |at_ready: Result<Option<Vec<u8>>>, after: &str| -> (usize, FakeClock) {
            let mut clock = FakeClock::new(start);
            let now = clock.shared();
            let mut at_ready = Some(at_ready);
            let after = after.to_owned();
            let startup = startup.clone();
            let mut read_log = move || -> Result<Option<Vec<u8>>> {
                let elapsed = now.get().saturating_duration_since(start);
                if elapsed < ready_at {
                    some_log(&startup)
                } else if elapsed == ready_at {
                    at_ready
                        .take()
                        .expect("the ready-instant read happens once")
                } else {
                    some_log(&after)
                }
            };
            let offset = wait_for_startup_readiness_with(
                &mut read_log,
                deadline,
                WINDOWS_STARTUP_READINESS_TIMING,
                poll,
                &mut clock,
            )
            .unwrap();
            (offset, clock)
        };

        // The pre-paste read carries a fresh reload: no paste at `ready_at`; the
        // reload owes its own quiet period, and the offset covers the reload line.
        let (offset, clock) = waited(some_log(&reloaded), &reloaded);
        assert_eq!(clock.slept, ready_at + STARTUP_QUIET_PERIOD);
        assert_eq!(offset, reloaded.len());
        let receipt = framed_receipt_line("hello", &pending);
        assert_eq!(
            observe_input_receipt(
                Some((reloaded.clone() + &receipt).as_bytes()),
                offset,
                &pending
            ),
            ReceiptEvidence::Delivered,
            "the later paste is confirmed by a receipt after the reload line"
        );
        assert_eq!(
            observe_input_receipt(Some(reloaded.as_bytes()), startup.len(), &pending),
            ReceiptEvidence::NoReceipt {
                appended: fresh_reload.len(),
                partial_tail: false
            },
            "the reload line is not evidence at the round-5 offset either"
        );

        // The pre-paste read finds no log: a discontinuity, never offset zero; the
        // reappearing log is a new observation with its own quiet period.
        let (offset, clock) = waited(Ok(None), &startup);
        assert_eq!(clock.slept, ready_at + poll + STARTUP_QUIET_PERIOD);
        assert_eq!(offset, startup.len());

        // The pre-paste read is a re-stamped replacement of the same length: the
        // same indices, but a discontinuity that restarts the quiet period.
        let (offset, clock) = waited(some_log(&restamped), &restamped);
        assert_eq!(clock.slept, ready_at + STARTUP_QUIET_PERIOD);
        assert_eq!(offset, restamped.len());

        // The pre-paste read is a plain continuation: pasted at once at its length,
        // including a partial line appended since the previous read.
        let grown = startup.clone() + "I0924 16:42:33.000000     560 manager.go:1308] Reloading";
        let (offset, clock) = waited(some_log(&grown), &grown);
        assert_eq!(clock.slept, ready_at);
        assert_eq!(offset, grown.len());
    }

    #[test]
    fn follow_up_pre_paste_offset_is_the_log_length_and_never_zero_for_a_missing_log() {
        let startup = successful_startup_log();
        assert_eq!(
            follow_up_pre_paste_offset(Some(startup.as_bytes())).unwrap(),
            startup.len()
        );
        assert_eq!(follow_up_pre_paste_offset(Some(b"")).unwrap(), 0);
        let error = follow_up_pre_paste_offset(None).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("agy.log is missing before the console paste"));
        assert!(message.contains("the follow-up was not pasted"));
    }

    #[test]
    fn log_replay_anchors_its_origin_to_the_first_timestamped_line() {
        // A glog file header before the first timestamped line: the replay used to
        // take zero as the origin, scheduling the startup line 16 h 42 min after
        // `start` and past the 300 s replay deadline.
        let header = "Log line format: [IWEF]mmdd hh:mm:ss.uuuuuu threadid file:line] msg\n";
        assert_eq!(glog_time_of_day(header.trim_end()), None);
        let start = Instant::now();
        let replay = LogReplay::new(&format!("{header}{REAL_SUCCESS_STARTUP}"), start);
        assert_eq!(replay.lines[0], (start, header.to_owned()));
        assert_eq!(
            replay.lines[1].0, start,
            "the first timestamped line is the origin"
        );
        assert!(replay.lines[1].1.starts_with("I0924 16:42:24.068931"));
        let (startup_at, startup_line) = replay
            .lines
            .iter()
            .find(|(_, line)| line.contains(STARTUP_COMPLETED_MARKER))
            .unwrap();
        assert!(startup_line.starts_with("I0924 16:42:24.080467"));
        assert_eq!(
            *startup_at,
            start + Duration::from_micros(11_536),
            "16:42:24.080467 is 11.536 ms after the first timestamp 16:42:24.068931"
        );
        assert_eq!(*startup_at, replay.recorded("16:42:24.080467"));
        let visible = String::from_utf8(replay.visible_at(start).unwrap()).unwrap();
        assert!(visible.starts_with(header));
        assert!(visible.contains("16:42:24.068931"));
        assert!(!visible.contains(STARTUP_COMPLETED_MARKER));

        // session-udT6uY: the gate is ready 3.5 s after the last plain reload at
        // 16:42:29.104592, whether or not the log starts with a header.
        let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "16:42:32.604592",
            "session-udT6uY with a header",
        );
        let without_header = LogReplay::new(REAL_SUCCESS_STARTUP, start);
        assert_eq!(
            replay_readiness(&without_header, WINDOWS_STARTUP_READINESS_TIMING),
            ready
        );
    }

    // The recorded lost pastes (2026-09-24, Agy 1.2.10, Windows console) under the
    // rule in force and under the rules of their time. Each paste landed on the
    // workspace-trust dialog, and the trust reload these logs show is what its Enter
    // caused. The gate cannot see the dialog: it is ready one quiet period after the
    // startup burst, before that reload in every one of them, and a window only moved
    // the paste, and the loss, later. What withholds these pastes is the trust
    // evidence, which none of these logs holds before its paste
    // (`workspace_customization_load_is_read_from_the_sessions_own_log`).
    #[test]
    fn readiness_gate_is_ready_before_the_trust_reload_of_the_lost_fixture() {
        // session-fMqSQc: ready at 16:41:17.24, 3.5 s after the 16:41:13 reload; the
        // fixed 12 s delay pasted at about 16:41:19, and the trust reload followed at
        // 16:41:20.81, 13.0 s after startup.
        let log = late_reload_startup_log() + &late_reload_completion();
        let replay = LogReplay::new(&log, Instant::now());
        let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, ready, "16:41:17.237805", "session-fMqSQc");
        assert!(
            ready < replay.recorded("16:41:20.813553"),
            "the gate is ready before the trust reload"
        );
        assert!(
            !workspace_customizations_loaded(late_reload_startup_log().as_bytes()),
            "the trust evidence is missing before the paste"
        );

        // The 45 s rule on the same recording: the reload falls inside its window, so
        // it would have been ready 3.5 s after the reload's hooks line. The recording
        // holds that reload only because an earlier rule had pasted.
        let former = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            former,
            "16:41:24.314059",
            "session-fMqSQc, the 45 s rule",
        );
        assert!(former > replay.recorded("16:41:20.816140"));
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "16:41:07.876154",
                    "16:41:17.237805",
                    "16:41:20.814059",
                    "16:41:24.314058",
                    "16:41:24.314059",
                ]
            ),
            vec![
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Ready,
            ]
        );

        // A skills reload during the quiet period restarts the period; a hooks line
        // after it is never demanded.
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut gate = ReadinessGate::new(start, WINDOWS_STARTUP_READINESS_TIMING);
        let late = late_reload_startup_log();
        assert_eq!(
            gate.observe(Some(late.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        let reloading = late.clone()
            + &glog("16:41:15.000000", 410, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:41:15.000000", 410, "manager.go:1308", SLASH_RELOAD);
        assert_eq!(
            gate.observe(Some(reloading.as_bytes()), at(1_300)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(reloading.as_bytes()), at(4_700)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(reloading.as_bytes()), at(4_800)),
            ReadinessState::Ready,
            "silence after a reload without a hooks line is readiness"
        );
    }

    #[test]
    fn readiness_gate_is_ready_where_the_recorded_paste_met_the_trust_dialog() {
        // session-IEKjtC: ready at 17:47:13.12, 3.5 s after the 17:47:09.62 plain
        // reload, which is where the round-3 build pasted. The trust reload at
        // 17:47:13.468 (9.8 s after startup) followed that paste by 0.35 s and no
        // receipt came.
        let replay = LogReplay::new(REAL_DEFERRED_RELOAD_STARTUP, Instant::now());
        let trust_reload = replay.recorded("17:47:13.468286");
        let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, ready, "17:47:13.116209", "session-IEKjtC");
        assert!(
            ready < trust_reload,
            "the gate is ready before the trust reload"
        );

        // The 45 s rule on the same recording waits past the reload, its hooks line,
        // and its completion.
        let former = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            former,
            "17:47:16.968806",
            "session-IEKjtC, the 45 s rule",
        );
        assert!(former > replay.recorded("17:47:13.470456"));
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "17:47:03.620082",
                    "17:47:03.666570",
                    "17:47:06.421536",
                    "17:47:08.894558",
                    "17:47:09.616209",
                    "17:47:13.116209",
                    "17:47:13.468806",
                    "17:47:16.968805",
                    "17:47:16.968806",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Ready,
            ]
        );
    }

    #[test]
    fn a_20_s_window_only_moved_the_paste_onto_the_trust_dialog_later() {
        // session-ql5TVc: the quiet period ended at 18:48:12.15, 3.5 s after the
        // 18:48:08.65 plain reload, which is when the rule in force is ready. The
        // round-6 rule's 20 s window ended at 18:48:24.12, when it pasted, and the
        // trust reload at 18:48:25.481988 (21.4 s after startup) followed that paste
        // with no receipt. The 45 s rule on the recording is ready 3.5 s after the
        // reload's hooks line.
        let replay = LogReplay::new(REAL_LATE_DEFERRED_RELOAD_STARTUP, Instant::now());
        let startup = replay.recorded("18:48:04.115440");
        let deferred_reload = replay.recorded("18:48:25.481988");
        let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, in_force, "18:48:12.146591", "session-ql5TVc");
        assert!(in_force < deferred_reload);
        assert!(
            deferred_reload - startup > Duration::from_secs(21)
                && deferred_reload - startup < DEFERRED_RELOAD_WINDOW,
            "the reload arrived 21.4 s after startup, inside the 45 s window"
        );
        let round_6 = replay_readiness(&replay, ROUND_6_TIMING);
        assert_ready_at(
            &replay,
            round_6,
            "18:48:24.115440",
            "session-ql5TVc, round 6",
        );
        assert!(
            round_6 < deferred_reload,
            "the 20 s window pasted before the deferred reload"
        );
        let ready = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "18:48:28.982515",
            "session-ql5TVc, the 45 s rule",
        );
        assert!(
            ready > replay.recorded("18:48:25.485154"),
            "the 45 s window waits past the deferred reload, its hooks line, and its completion"
        );
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "18:48:04.115440",
                    "18:48:04.161713",
                    "18:48:05.977318",
                    "18:48:08.646591",
                    "18:48:12.146591",
                    "18:48:24.115440",
                    "18:48:25.482515",
                    "18:48:28.982514",
                    "18:48:28.982515",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Ready,
            ]
        );
        assert_eq!(
            replay.states(
                ROUND_6_TIMING,
                &[
                    "18:48:04.115440",
                    "18:48:04.161713",
                    "18:48:05.977318",
                    "18:48:08.646591",
                    "18:48:12.146591",
                    "18:48:24.115439",
                    "18:48:24.115440",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Ready
            ],
            "the round-6 window ended 1.3 s before the reload"
        );
    }

    // The recorded delivered pastes of 2026-09-24 (Agy 1.2.10, Windows console): the
    // workspace was trusted before launch, the customization load is logged at
    // startup, and no trust reload ever follows. The rule in force is ready one quiet
    // period after the last plain reload; the 45 s rule waited out its window for a
    // reload that was not coming.
    #[test]
    fn readiness_gate_is_ready_after_the_quiet_period_where_the_recorded_pastes_were_delivered() {
        // session-udT6uY: the skills reload 0.5 ms after `CLI startup completed` is
        // the startup reload, with the customization load right behind it. Ready
        // 3.5 s after the last plain reload at 16:42:29.10; the paste of the fixed
        // 12 s delay was received at 16:42:50.21. The 45 s window ended at
        // 16:43:09.08.
        assert!(workspace_customizations_loaded(
            REAL_SUCCESS_STARTUP.as_bytes()
        ));
        let replay = LogReplay::new(REAL_SUCCESS_STARTUP, Instant::now());
        let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, in_force, "16:42:32.604592", "session-udT6uY");
        let ready = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "16:43:09.080467",
            "session-udT6uY, the 45 s rule",
        );
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "16:42:24.080467",
                    "16:42:24.127839",
                    "16:42:25.848949",
                    "16:42:28.530216",
                    "16:42:29.104592",
                    "16:42:32.604592",
                    "16:43:09.080466",
                    "16:43:09.080467",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Ready,
            ]
        );

        // session-M8QFPp: the same shape 1.6 ms after startup. The round-8 rule took
        // the startup reload for the deferred one and pasted at 20:37:49.15, 3.5 s
        // after the last plain reload; that paste was received. It is the instant of
        // the rule in force. The 45 s window ended at 20:38:25.65.
        assert!(workspace_customizations_loaded(
            REAL_STARTUP_RELOAD_AFTER_STARTUP.as_bytes()
        ));
        let replay = LogReplay::new(REAL_STARTUP_RELOAD_AFTER_STARTUP, Instant::now());
        let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(
            &replay,
            in_force,
            "20:37:49.146941",
            "session-M8QFPp (the instant of the recorded, delivered paste)",
        );
        let ready = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "20:38:25.653251",
            "session-M8QFPp, the 45 s rule",
        );
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "20:37:40.653251",
                    "20:37:40.700886",
                    "20:37:45.646941",
                    "20:37:49.146941",
                    "20:38:25.653250",
                    "20:38:25.653251",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Ready,
            ]
        );
    }

    #[test]
    fn a_35_s_window_only_moved_the_paste_onto_the_trust_dialog_later() {
        // session-uqraap: the quiet period ended at 20:44:09.93, 3.5 s after the
        // 20:44:06.43 plain reload, which is when the rule in force is ready. The
        // round-8 rule's 35 s window ended at 20:44:36.44, when it pasted, and the
        // trust reload at 20:44:37.855815 (36.4 s after startup) followed that paste
        // with no receipt. The 45 s rule on the recording is ready 3.5 s after the
        // reload's hooks line.
        let replay = LogReplay::new(REAL_SECOND_CLUSTER_RELOAD_STARTUP, Instant::now());
        let startup = replay.recorded("20:44:01.443214");
        let deferred_reload = replay.recorded("20:44:37.855815");
        let in_force = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
        assert_ready_at(&replay, in_force, "20:44:09.927564", "session-uqraap");
        assert!(in_force < deferred_reload);
        assert!(
            deferred_reload - startup > ROUND_8_TIMING.deferred_reload_window
                && deferred_reload - startup < DEFERRED_RELOAD_WINDOW,
            "the reload arrived 36.4 s after startup, past the 35 s window and inside the 45 s window"
        );
        let round_8 = replay_readiness(&replay, ROUND_8_TIMING);
        assert_ready_at(
            &replay,
            round_8,
            "20:44:36.443214",
            "session-uqraap, round 8",
        );
        assert!(
            round_8 < deferred_reload,
            "the 35 s window pasted before the deferred reload"
        );
        let ready = replay_readiness(&replay, ROUND_9_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "20:44:41.356320",
            "session-uqraap, the 45 s rule",
        );
        assert!(
            ready > replay.recorded("20:44:37.857983"),
            "the 45 s window waits past the deferred reload, its hooks line, and its completion"
        );
        assert_eq!(
            replay.states(
                ROUND_9_TIMING,
                &[
                    "20:44:01.443214",
                    "20:44:01.490907",
                    "20:44:04.176592",
                    "20:44:06.427564",
                    "20:44:09.927564",
                    "20:44:36.443214",
                    "20:44:37.856320",
                    "20:44:41.356319",
                    "20:44:41.356320",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Settling,
                ReadinessState::Settling,
                ReadinessState::Ready,
            ]
        );
        assert_eq!(
            replay.states(
                ROUND_8_TIMING,
                &[
                    "20:44:01.443214",
                    "20:44:09.927564",
                    "20:44:36.443213",
                    "20:44:36.443214",
                ]
            ),
            vec![
                ReadinessState::AwaitingRedraw,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::AwaitingDeferredReload,
                ReadinessState::Ready
            ],
            "the round-8 window ended 1.4 s before the reload"
        );
    }

    // session-M8QFPp: the paste at 20:37:49.15 was accepted, and Agy logged its
    // HandleUserInput receipt at 20:38:07.152165, 18.0 s later, under heavy CPU
    // load. The former 15 s receipt window had ended, so the adapter reported
    // delivery-uncertain for a delivered paste; the 60 s window sees the receipt.
    #[test]
    fn receipt_watch_outlasts_the_slow_processing_observed_under_load() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let startup = REAL_STARTUP_RELOAD_AFTER_STARTUP.to_owned();
        let receipt_latency = glog_timestamp("I0924 20:38:07.152165     595 input_loop.go:107] x")
            .unwrap()
            - glog_timestamp("I0924 20:37:49.146941       1 x.go:1] x").unwrap();
        assert!(receipt_latency > ROUND_8_RECEIPT_WINDOW);
        assert!(receipt_latency < INPUT_RECEIPT_WINDOW);
        let framed = terminal_correlated_prompt("hello", &pending, true).unwrap();
        let receipt = format!(
            "I0924 20:38:07.152165     595 input_loop.go:107] HandleUserInput called with text: {}\n",
            go_quoted(&framed)
        );
        let pasted_at = Instant::now();
        let mut clock = FakeClock::new(pasted_at);
        let now = clock.shared();
        let mut read_log = {
            let startup = startup.clone();
            move || -> Result<Option<Vec<u8>>> {
                if now.get().saturating_duration_since(pasted_at) < receipt_latency {
                    some_log(&startup)
                } else {
                    some_log(&(startup.clone() + &receipt))
                }
            }
        };
        confirm_input_receipt_with(
            &mut read_log,
            &pending,
            startup.len(),
            pasted_at,
            pasted_at + Duration::from_secs(300),
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert!(clock.slept >= receipt_latency);
        assert!(clock.slept < receipt_latency + Duration::from_millis(100));
        assert!(
            clock.slept > ROUND_8_RECEIPT_WINDOW,
            "the round-8 window had ended before the receipt"
        );
    }

    #[test]
    fn readiness_gate_deadline_report_names_every_missing_marker() {
        let start = Instant::now();
        let poll = Duration::from_millis(100);
        let deadline = start + Duration::from_secs(1);
        let no_quiet_period = ReadinessTiming {
            quiet_period: Duration::ZERO,
            ..WINDOWS_STARTUP_READINESS_TIMING
        };

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![Ok(None)]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Agy did not report startup readiness before the deadline"));
        assert!(message.contains("agy.log has not been created"));
        assert!(message.contains(&format!(
            "missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}, {QUIET_PERIOD_DESCRIPTION}; the quiet period is 3500 ms, timed from the newest reload, redraw, or hooks line; the prompt was not pasted"
        )));
        assert!(
            !message.contains("deferred reload"),
            "the rule in force has no window to report"
        );
        assert!(!message.contains("hooks completion"));
        assert_eq!(clock.slept, Duration::from_secs(1));

        // A rule with a window names the window as well: the former 45 s rule.
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![Ok(None)]),
            deadline,
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&format!(
            "missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}, {DEFERRED_RELOAD_DESCRIPTION}, {QUIET_PERIOD_DESCRIPTION}"
        )));
        assert!(
            message
                .contains("the quiet period is 3500 ms and the deferred reload window is 45000 ms, timed concurrently")
        );
        assert!(message.contains("the prompt was not pasted"));

        let startup_only = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        );
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_only)]),
            deadline,
            no_quiet_period,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("no `Full redraw completed` line after `CLI startup completed`"));
        assert!(!message.contains("missing markers: `CLI startup completed`"));
        assert!(message.contains(&format!(
            "missing markers: {REDRAW_AFTER_STARTUP_DESCRIPTION}; the quiet period"
        )));

        let redraw_only = glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&redraw_only)]),
            deadline,
            no_quiet_period,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("agy.log has no `CLI startup completed` line"));
        assert!(message.contains(&format!(
            "missing markers: `CLI startup completed`, {REDRAW_AFTER_STARTUP_DESCRIPTION}; the quiet period"
        )));

        // Startup and redraw, no skills reload after them, under the former 45 s
        // rule: the window and the quiet period are both missing at 1 s, only the
        // window at 5 s.
        let startup_redraw = startup_only.clone() + &redraw_only;
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            deadline,
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(
            "agy.log has no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and the deferred reload window since `CLI startup completed` has not elapsed"
        ));
        assert!(message.contains(&format!(
            "missing markers: {DEFERRED_RELOAD_DESCRIPTION}, {QUIET_PERIOD_DESCRIPTION}; the quiet period"
        )));
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            start + Duration::from_secs(5),
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&format!(
            "missing markers: {DEFERRED_RELOAD_DESCRIPTION}; the quiet period"
        )));

        // session-fMqSQc with its trust reload logged: only the quiet period is
        // missing at 1 s.
        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&settled_startup_log())]),
            deadline,
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("logged a reload, redraw, or hooks line within the quiet period"));
        assert!(message.contains(&format!(
            "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period"
        )));

        // Startup seen at once, the redraw a poll later, no skills reload: ready one
        // quiet period after the redraw was first seen, at 3.6 s. The former 45 s
        // rule was ready when its window ended, long after that.
        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_only), some_log(&startup_redraw)]),
            start + Duration::from_secs(60),
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, poll + STARTUP_QUIET_PERIOD);
        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_only), some_log(&startup_redraw)]),
            start + Duration::from_secs(60),
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, Duration::from_secs(45));

        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![Err(anyhow::anyhow!(
                "refusing non-regular session file"
            ))]),
            deadline,
            no_quiet_period,
            poll,
            &mut FakeClock::new(start),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("startup readiness could not be observed"));
    }

    // The rule, stated as deadlines: with startup and its redraw already logged and
    // nothing after them, the quiet period ends at 3.5 s and the gate is ready then.
    // A deadline past it reaches the paste; a shorter one fails `not_sent` naming the
    // quiet period as the only missing condition.
    #[test]
    fn readiness_wait_pastes_when_the_quiet_period_ends_and_not_before() {
        let start = Instant::now();
        let poll = Duration::from_millis(100);
        let startup_redraw = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        ) + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);

        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            start + Duration::from_secs(4),
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, startup_redraw.len());
        assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            start + Duration::from_secs(3),
            WINDOWS_STARTUP_READINESS_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        assert_eq!(clock.slept, Duration::from_secs(3));
        let message = format!("{error:#}");
        assert!(message.contains("logged a reload, redraw, or hooks line within the quiet period"));
        assert!(message.contains(&format!(
            "missing markers: {QUIET_PERIOD_DESCRIPTION}; the quiet period is 3500 ms, timed from the newest reload, redraw, or hooks line; the prompt was not pasted"
        )));
        let failure = terminal::TerminalSendFailure::not_sent(error);
        assert!(
            !failure.delivery_may_have_occurred(),
            "a gate that never passed is not_sent, never uncertain"
        );
    }

    // The former 45 s rule on the same log: the quiet period ended at 3.5 s and the
    // window at 45 s, so that rule was ready at exactly 45 s. A deadline one second
    // past the window reached the paste; a deadline one second short of it failed
    // `not_sent` naming the window as the only missing condition.
    #[test]
    fn the_former_window_rule_pasted_at_the_window_end_and_not_before() {
        let start = Instant::now();
        let poll = Duration::from_millis(100);
        let startup_redraw = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        ) + &glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW);

        let mut clock = FakeClock::new(start);
        let offset = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            start + Duration::from_secs(46),
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(offset, startup_redraw.len());
        assert_eq!(
            clock.slept, DEFERRED_RELOAD_WINDOW,
            "ready at startup + 45 s, not + 45 s + the quiet period"
        );

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup_redraw)]),
            start + Duration::from_secs(44),
            ROUND_9_TIMING,
            poll,
            &mut clock,
        )
        .unwrap_err();
        assert_eq!(clock.slept, Duration::from_secs(44));
        let message = format!("{error:#}");
        assert!(message.contains(
            "agy.log has no `Reloading system slash commands and skills` line stamped at least 1 s after `CLI startup completed` and the deferred reload window since `CLI startup completed` has not elapsed"
        ));
        assert!(message.contains(&format!(
            "missing markers: {DEFERRED_RELOAD_DESCRIPTION}; the quiet period is 3500 ms and the deferred reload window is 45000 ms, timed concurrently"
        )));
        assert!(
            !message.contains(QUIET_PERIOD_DESCRIPTION),
            "the quiet period ended at 3.5 s and is not missing"
        );
        assert!(message.contains("the prompt was not pasted"));
        let failure = terminal::TerminalSendFailure::not_sent(error);
        assert!(
            !failure.delivery_may_have_occurred(),
            "a gate that never passed is not_sent, never uncertain"
        );
    }

    // Live observation (session-udT6uY and the round-7 asks): the skills reload that
    // Agy logs milliseconds after `HandleUserInput` and the conversation-start lines
    // is triggered by the new conversation, not the deferred startup reload. The
    // receipt watch returns on the first read that holds the receipt, so the reload
    // can neither delay nor revoke the delivered classification.
    #[test]
    fn receipt_stays_delivered_when_the_conversation_reload_follows_it() {
        let pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
        let startup = successful_startup_log();
        let with_receipt = format!("{startup}{REAL_SUCCESS_RECEIPT_HEAD}\n");
        let with_reload = format!("{with_receipt}{REAL_SUCCESS_AFTER_RECEIPT}");
        assert!(with_reload[with_receipt.len()..].contains(SKILLS_RELOAD_MARKER));
        assert_eq!(
            observe_input_receipt(Some(with_reload.as_bytes()), startup.len(), &pending),
            ReceiptEvidence::Delivered
        );

        let pasted_at = Instant::now();
        let mut clock = FakeClock::new(pasted_at);
        confirm_input_receipt_with(
            &mut log_sequence(vec![some_log(&startup), some_log(&with_reload)]),
            &pending,
            startup.len(),
            pasted_at,
            pasted_at + Duration::from_secs(120),
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, Duration::from_millis(100));
    }

    #[test]
    fn input_receipt_for_a_long_prompt_carries_the_whole_marker_near_the_front() {
        let pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
        let prompt = long_markdown_prompt();
        assert!(prompt.len() >= 1600);
        let framed = terminal_correlated_prompt(&prompt, &pending, true).unwrap();
        let quoted = go_quoted(&framed);
        let line = receipt_line(&quoted);
        // Agy 1.2.10 logged this shape unabridged for a 1.6 KB prompt: a 2.7 KB line
        // ending in the quoted request's closing `\"` plus the Go closing quote.
        assert!(line.len() > framed.len());
        assert!(line.trim_end().ends_with("\\\"\""));
        let marker_at = line.find(&pending.marker).unwrap();
        let prefix_at = line.find(WINDOWS_PROTOCOL_PREFIX).unwrap();
        assert!(prefix_at < marker_at && marker_at < 400);

        let receipts = input_receipts(line.as_bytes());
        assert_eq!(receipts.len(), 1);
        assert!(!receipts[0].truncated);
        assert_eq!(receipts[0].text, quoted[1..quoted.len() - 1]);
        assert!(receipt_matches(&receipts[0], &pending));
        assert_eq!(
            observe_input_receipt(Some(line.as_bytes()), 0, &pending),
            ReceiptEvidence::Delivered
        );

        // The real receipt head starts with the same prefix and marker.
        let real = format!("{REAL_SUCCESS_RECEIPT_HEAD}\n");
        let receipts = input_receipts(real.as_bytes());
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].truncated, "the fixture is abridged");
        assert!(receipt_matches(&receipts[0], &pending));
        let real_marker_at = REAL_SUCCESS_RECEIPT_HEAD.find(&pending.marker).unwrap();
        let real_prefix_at = REAL_SUCCESS_RECEIPT_HEAD
            .find(WINDOWS_PROTOCOL_PREFIX)
            .unwrap();
        assert!(real_prefix_at < real_marker_at && real_marker_at < 400);

        let legacy = format!(
            "ERROR: logging before google.Init: I0827 22:11:28.641211     406 input_loop.go:36] HandleUserInput called with text: {}\n",
            go_quoted(&framed)
        );
        assert_eq!(
            observe_input_receipt(Some(legacy.as_bytes()), 0, &pending),
            ReceiptEvidence::Delivered
        );

        let manual = receipt_line(&go_quoted(&format!("please finish {}", pending.marker)));
        assert_eq!(
            observe_input_receipt(Some(manual.as_bytes()), 0, &pending),
            ReceiptEvidence::NoReceipt {
                appended: manual.len(),
                partial_tail: false
            },
            "a manual turn without the protocol prefix is not this delivery"
        );
    }

    #[test]
    fn input_receipt_requires_the_complete_marker_and_never_a_colliding_prefix() {
        let old = PendingAgyTurn::new("28404-1790235743098225800-1").unwrap();
        let new = PendingAgyTurn::new("28404-1790235743098225800-10").unwrap();
        assert!(new.marker.starts_with(old.marker.trim_end_matches(" -->")));

        // Turn 1 was delivered and its receipt is complete.
        let old_receipt = framed_receipt_line("first", &old);
        let log = successful_startup_log() + &old_receipt;
        assert_eq!(
            observe_input_receipt(Some(log.as_bytes()), 0, &old),
            ReceiptEvidence::Delivered
        );
        assert_eq!(
            observe_input_receipt(Some(log.as_bytes()), 0, &new),
            ReceiptEvidence::NoReceipt {
                appended: log.len(),
                partial_tail: false
            },
            "the complete old marker never confirms the new turn"
        );

        // Turn 10 is pasted after the offset; the old receipt is before it.
        let pre_paste_len = log.len();
        assert_eq!(
            observe_input_receipt(Some(log.as_bytes()), pre_paste_len, &new),
            ReceiptEvidence::NoReceipt {
                appended: 0,
                partial_tail: false
            }
        );
        // A receipt that Agy cut right after the shared token prefix confirms neither
        // turn: the complete marker is not visible.
        let old_framed = terminal_correlated_prompt("first", &old, true).unwrap();
        let old_quoted = go_quoted(&old_framed);
        let cut = old_quoted.find(&old.marker).unwrap() + old.marker.len() - " -->".len();
        let truncated_old = receipt_line(&format!("{}...\"", &old_quoted[..cut]));
        assert!(truncated_old.contains("-1...\""));
        let receipts = input_receipts(truncated_old.as_bytes());
        assert!(receipts[0].truncated);
        assert!(!receipt_matches(&receipts[0], &old));
        assert!(!receipt_matches(&receipts[0], &new));
        let with_truncated = log.clone() + &truncated_old;
        assert_eq!(
            observe_input_receipt(Some(with_truncated.as_bytes()), pre_paste_len, &new),
            ReceiptEvidence::NoReceipt {
                appended: truncated_old.len(),
                partial_tail: false
            }
        );
        assert_eq!(
            observe_input_receipt(Some(with_truncated.as_bytes()), pre_paste_len, &old),
            ReceiptEvidence::NoReceipt {
                appended: truncated_old.len(),
                partial_tail: false
            }
        );

        // Only the complete new marker after the offset is the new turn's receipt.
        let new_receipt = framed_receipt_line("second", &new);
        let delivered = log.clone() + &new_receipt;
        assert_eq!(
            observe_input_receipt(Some(delivered.as_bytes()), pre_paste_len, &new),
            ReceiptEvidence::Delivered
        );
        // The same marker before the offset is not evidence for this submission.
        assert_eq!(
            observe_input_receipt(Some(delivered.as_bytes()), delivered.len(), &new),
            ReceiptEvidence::NoReceipt {
                appended: 0,
                partial_tail: false
            }
        );
        let different = new_receipt.replace("-10 -->", "-17 -->");
        let other = log + &different;
        assert_eq!(
            observe_input_receipt(Some(other.as_bytes()), pre_paste_len, &new),
            ReceiptEvidence::NoReceipt {
                appended: different.len(),
                partial_tail: false
            }
        );
    }

    #[test]
    fn input_receipt_evidence_only_counts_lines_that_start_after_the_offset() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let receipt = framed_receipt_line("hello", &pending);

        // The pre-paste snapshot ended inside an unrelated line.
        let snapshot = successful_startup_log()
            + "I0924 16:42:49.000000     590 quota_manager.go:45] doRefreshQuota";
        let log = snapshot.clone() + ": starting reload (force=false)\n" + &receipt;
        assert_eq!(
            evidence_start(log.as_bytes(), snapshot.len()),
            Some(snapshot.len() + ": starting reload (force=false)\n".len())
        );
        assert_eq!(
            observe_input_receipt(Some(log.as_bytes()), snapshot.len(), &pending),
            ReceiptEvidence::Delivered
        );
        let unfinished = snapshot.clone() + ": starting";
        assert_eq!(evidence_start(unfinished.as_bytes(), snapshot.len()), None);
        assert_eq!(
            observe_input_receipt(Some(unfinished.as_bytes()), snapshot.len(), &pending),
            ReceiptEvidence::NoReceipt {
                appended: ": starting".len(),
                partial_tail: true
            }
        );

        // A line that straddles the offset started before the paste and is never
        // evidence, even when its tail carries the marker.
        let (head, tail) = receipt
            .split_at(receipt.find(INPUT_RECEIPT_MARKER).unwrap() + INPUT_RECEIPT_MARKER.len());
        let snapshot = successful_startup_log() + head;
        let straddling = snapshot.clone() + tail;
        assert_eq!(
            observe_input_receipt(Some(straddling.as_bytes()), 0, &pending),
            ReceiptEvidence::Delivered
        );
        assert_eq!(
            observe_input_receipt(Some(straddling.as_bytes()), snapshot.len(), &pending),
            ReceiptEvidence::NoReceipt {
                appended: tail.len(),
                partial_tail: false
            }
        );

        // Real sequence: the readiness startup, then the delivered receipt.
        let real = format!(
            "{REAL_SUCCESS_STARTUP}{REAL_SUCCESS_RECEIPT_HEAD}\n{REAL_SUCCESS_AFTER_RECEIPT}"
        );
        let real_pending = PendingAgyTurn::new(REAL_SUCCESS_TOKEN).unwrap();
        assert_eq!(
            observe_input_receipt(
                Some(real.as_bytes()),
                REAL_SUCCESS_STARTUP.len(),
                &real_pending
            ),
            ReceiptEvidence::Delivered
        );
        assert_eq!(
            observe_input_receipt(Some(real.as_bytes()), real.len(), &real_pending),
            ReceiptEvidence::NoReceipt {
                appended: 0,
                partial_tail: false
            }
        );
    }

    #[test]
    fn receipt_watch_confirms_only_a_complete_receipt_after_the_offset() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let startup = successful_startup_log();
        let receipt = framed_receipt_line("hello", &pending);
        let pasted_at = Instant::now();
        let deadline = pasted_at + Duration::from_secs(120);
        let poll = Duration::from_millis(100);

        let mut clock = FakeClock::new(pasted_at);
        confirm_input_receipt_with(
            &mut log_sequence(vec![
                some_log(&startup),
                some_log(&(startup.clone() + &receipt)),
            ]),
            &pending,
            startup.len(),
            pasted_at,
            deadline,
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, poll);

        let now = Instant::now();
        assert_eq!(
            input_receipt_window_end(now, now + Duration::from_secs(60)),
            now + INPUT_RECEIPT_WINDOW
        );
        assert_eq!(
            input_receipt_window_end(now, now + Duration::from_secs(3)),
            now + Duration::from_secs(3)
        );
    }

    #[test]
    fn receipt_watch_never_reports_not_sent_after_the_paste() {
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let startup = successful_startup_log();
        let receipt = framed_receipt_line("hello", &pending);
        let pasted_at = Instant::now();
        let deadline = pasted_at + Duration::from_secs(120);
        let poll = Duration::from_secs(5);
        let uncertain = |logs: Vec<Result<Option<Vec<u8>>>>,
                         pre_paste_len: usize,
                         deadline: Instant|
         -> (String, FakeClock) {
            let mut clock = FakeClock::new(pasted_at);
            let failure = confirm_input_receipt_with(
                &mut log_sequence(logs),
                &pending,
                pre_paste_len,
                pasted_at,
                deadline,
                poll,
                &mut clock,
            )
            .unwrap_err();
            assert!(
                failure.delivery_may_have_occurred(),
                "a paste without a receipt is never not_sent"
            );
            let message = format!("{:#}", failure.error());
            assert!(message.contains(&pending.marker));
            assert!(message.contains("may have been accepted and is not repeated"));
            (message, clock)
        };

        // Continuous, complete log with no receipt for the whole window: no marker
        // proves the input was drained, so non-delivery is not proven.
        let (message, clock) = uncertain(vec![some_log(&startup)], startup.len(), deadline);
        assert!(message.contains("no HandleUserInput receipt in the 0 bytes appended"));
        assert!(message.contains("within 60 seconds"));
        assert!(message.contains("non-delivery cannot be proven"));
        assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);

        // The real failure log: the late reload after the discarded paste is the only
        // thing Agy wrote, and it is not a receipt.
        let failure_log = late_reload_startup_log();
        let (message, _) = uncertain(
            vec![some_log(&(failure_log.clone() + &late_reload_completion()))],
            failure_log.len(),
            deadline,
        );
        assert!(message.contains(&format!(
            "no HandleUserInput receipt in the {} bytes appended",
            late_reload_completion().len()
        )));
        assert!(message.contains("non-delivery cannot be proven"));

        // A receipt appended between the last read (the 13th, at 60 s with a 5 s poll)
        // and the deadline check is not
        // seen; the outcome is still uncertain, never not_sent.
        let mut reads = 0;
        let receipt_after_last_read = {
            let startup = startup.clone();
            let receipt = receipt.clone();
            move || {
                reads += 1;
                if reads <= 13 {
                    some_log(&startup)
                } else {
                    some_log(&(startup.clone() + &receipt))
                }
            }
        };
        let mut clock = FakeClock::new(pasted_at);
        let mut read_log = receipt_after_last_read;
        let failure = confirm_input_receipt_with(
            &mut read_log,
            &pending,
            startup.len(),
            pasted_at,
            deadline,
            poll,
            &mut clock,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);
        assert_eq!(
            observe_input_receipt(read_log().unwrap().as_deref(), startup.len(), &pending),
            ReceiptEvidence::Delivered,
            "the receipt landed right after the last read"
        );

        // A deadline-capped window that ends before 60 s elapsed.
        let (message, clock) = uncertain(
            vec![some_log(&startup)],
            startup.len(),
            pasted_at + Duration::from_secs(3),
        );
        assert!(
            message
                .contains("the deadline ended the receipt window after 3 of the 60 second window")
        );
        assert_eq!(clock.slept, Duration::from_secs(3));

        // Rotation or truncation below the pre-paste offset: uncertain at once.
        let (message, clock) = uncertain(
            vec![some_log(&startup[..startup.len() / 2])],
            startup.len(),
            deadline,
        );
        assert!(message.contains("rotated or truncated"));
        assert!(message.contains(&format!("below the pre-paste offset {}", startup.len())));
        assert_eq!(clock.slept, Duration::ZERO);

        // A partial trailing line may still become the receipt.
        let torn = startup.clone() + receipt.trim_end_matches('\n');
        let (message, _) = uncertain(vec![some_log(&torn)], startup.len(), deadline);
        assert!(message.contains("ends with a partial line"));

        // Missing and unreadable logs.
        let (message, clock) = uncertain(vec![Ok(None)], startup.len(), deadline);
        assert!(message.contains("agy.log is missing after the paste"));
        assert_eq!(clock.slept, INPUT_RECEIPT_WINDOW);
        let (message, clock) = uncertain(
            vec![Err(anyhow::anyhow!("refusing non-regular session file"))],
            startup.len(),
            deadline,
        );
        assert!(message.contains("agy.log is unreadable"));
        assert_eq!(clock.slept, Duration::ZERO);
    }

    #[test]
    fn doctor_reports_startup_readiness_and_the_pending_receipt_without_recovery() {
        use super::super::super::doctor::Availability;
        let check = input_receipt_check(None);
        assert_eq!(check.id, "agy_input_receipt");
        assert_eq!(check.availability, Availability::Unknown);
        assert_eq!(check.reason_code, "agy_log_unavailable");

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-doctor1");
        fs::create_dir_all(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_log_missing"
        );

        let log_path = directory.join(AGY_LOG_FILE);
        let startup_only = glog(
            "16:41:07.830345",
            1,
            "analytics.go:187",
            "CLI startup completed (took 1ms)",
        );
        fs::write(&log_path, &startup_only).unwrap();
        // The Windows console rule requires the redraw after startup.
        let check = input_receipt_check_for_platform(Some(&directory), true);
        assert_eq!(check.reason_code, "agy_startup_markers_missing");
        let check = serde_json::to_value(&check).unwrap();
        assert!(
            check["detail"]
                .as_str()
                .unwrap()
                .contains("Full redraw completed")
        );
        let evidence = check["evidence"].clone();
        assert_eq!(evidence["startup_completed"], true);
        assert_eq!(evidence["redraw_required"], true);
        assert_eq!(evidence["redraw_after_startup_observed"], false);
        assert_eq!(evidence["deferred_reload_after_startup_observed"], false);
        assert_eq!(evidence["startup_reload_after_startup_ignored"], false);
        assert_eq!(evidence["conversation_reload_after_startup_ignored"], false);
        assert_eq!(evidence["activity_lines"], 0);
        assert!(evidence.get("latest_reload_completed").is_none());
        // The macOS rule does not: startup alone completes the markers there.
        let check = input_receipt_check_for_platform(Some(&directory), false);
        assert_eq!(check.reason_code, "agy_no_input_receipt");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["redraw_required"], false);
        assert_eq!(evidence["redraw_after_startup_observed"], false);
        // The real macOS log: the conversation reload is reported as ignored, the
        // trust reload as observed (the evidence keeps its "deferred" field name),
        // and the smoke's lost paste left no receipt.
        fs::write(&log_path, REAL_MACOS_INITIAL_TURN).unwrap();
        let check = input_receipt_check_for_platform(Some(&directory), false);
        assert_eq!(check.reason_code, "agy_no_input_receipt");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["conversation_reload_after_startup_ignored"], true);
        assert_eq!(evidence["deferred_reload_after_startup_observed"], true);
        assert_eq!(
            input_receipt_check_for_platform(Some(&directory), true).reason_code,
            "agy_startup_markers_missing",
            "the same log never satisfies the Windows rule"
        );

        for log in [
            REAL_QUIET_STARTUP.to_owned(),
            REAL_DEFERRED_RELOAD_STARTUP.to_owned(),
            late_reload_startup_log(),
            settled_startup_log(),
            REAL_SECOND_CLUSTER_RELOAD_STARTUP.to_owned(),
            REAL_STARTUP_RELOAD_AFTER_STARTUP.to_owned(),
            successful_startup_log(),
        ] {
            fs::write(&log_path, log).unwrap();
            assert_eq!(
                input_receipt_check_for_platform(Some(&directory), true).reason_code,
                "agy_no_input_receipt"
            );
        }
        // session-udT6uY: the skills reload 0.5 ms after startup is reported as the
        // ignored startup reload, not as the deferred one.
        let evidence =
            serde_json::to_value(input_receipt_check_for_platform(Some(&directory), true)).unwrap()
                ["evidence"]
                .clone();
        assert_eq!(evidence["deferred_reload_after_startup_observed"], false);
        assert_eq!(evidence["startup_reload_after_startup_ignored"], true);

        fs::write(&log_path, settled_startup_log()).unwrap();
        let pending = claim_pending_turn(&directory);
        let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
        write!(log, "{}", framed_receipt_line("hello", &pending)).unwrap();
        drop(log);
        let check = input_receipt_check_for_platform(Some(&directory), true);
        assert_eq!(check.reason_code, "agy_input_receipt_observed");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["startup_completed"], true);
        assert_eq!(evidence["redraw_after_startup_observed"], true);
        assert_eq!(evidence["deferred_reload_after_startup_observed"], true);
        assert_eq!(evidence["startup_reload_after_startup_ignored"], false);
        assert_eq!(evidence["input_receipts"], 1);
        assert_eq!(evidence["last_receipt_truncated"], false);
        assert_eq!(evidence["pending_marker_received"], true);
        assert_eq!(evidence["pending_marker"], pending.marker);
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "working");
    }

    // The launcher path for a lost initial paste: the gate passes (here the startup
    // log of session-fMqSQc stays static, so its quiet period ends), the paste is
    // followed by the trust reload and no receipt, as it was when it met the trust
    // dialog, and the launcher records the delivery-uncertain reason without touching
    // the claim or the composer.
    #[test]
    fn lost_initial_paste_ends_delivery_uncertain_with_the_reason_in_status_json() {
        use super::super::super::TURN_CLAIM_FILE;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-fMqSQc");
        fs::create_dir_all(directory.join("events")).unwrap();
        update_status(&directory, SessionState::AwaitingInitialInput, None, None).unwrap();
        let mut claim = acquire_turn_claim(&directory).unwrap();
        let pending = install_pending_turn(&directory, claim.token()).unwrap();

        let startup = late_reload_startup_log();
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let pre_paste_len = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&startup)]),
            start + Duration::from_secs(300),
            WINDOWS_STARTUP_READINESS_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
        // The read that passed the gate is the pre-paste offset: the whole startup
        // log. The launcher marks the session working before the paste.
        assert_eq!(pre_paste_len, startup.len());
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let pasted_at = clock.now();
        let after_paste = startup.clone() + &late_reload_completion();
        let failure = confirm_input_receipt_with(
            &mut log_sequence(vec![some_log(&startup), some_log(&after_paste)]),
            &pending,
            pre_paste_len,
            pasted_at,
            start + Duration::from_secs(300),
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());

        {
            let delivery_error = failure.error();
            let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "working");
        let error = status
            .error
            .expect("the reason is recorded in status.error");
        assert!(error.contains(&format!(
            "Agy input receipt for turn marker {} was not confirmed",
            pending.marker
        )));
        assert!(error.contains("no HandleUserInput receipt in the"));
        assert!(error.contains("non-delivery cannot be proven"));
        assert!(error.contains("the console paste may have been accepted and is not repeated"));
        assert!(
            directory.join(TURN_CLAIM_FILE).exists(),
            "the turn claim stays until the caller closes or the target completes"
        );
        assert!(read_pending_turn(&directory).unwrap().is_some());
    }

    // The macOS follow-up rule on the real macOS log: the redraw is not required and
    // there is no window, so the log-only gate is ready one quiet period after the
    // newest activity line. That is before the instant at which the smoke pasted onto
    // the trust dialog: the log of an untrusted session shows nothing until the dialog
    // is answered, which is why the paste waits for Agy's trust store first. The
    // Windows rule never passes this log because no redraw line ever arrives.
    #[test]
    fn macos_follow_up_gate_needs_only_startup_and_a_quiet_period() {
        let observation = observe_startup(REAL_MACOS_INITIAL_TURN.as_bytes());
        assert_eq!(observation.startup_line, Some(REAL_MACOS_STARTUP_LINE));
        assert_eq!(
            observation.redraw_after_startup, None,
            "Agy logs no Full redraw completed on macOS"
        );
        assert_eq!(
            observation.conversation_reload_after_startup,
            Some(REAL_MACOS_CONVERSATION_RELOAD_LINE)
        );
        assert_eq!(
            observation.deferred_reload_after_startup,
            Some(REAL_MACOS_DEFERRED_RELOAD_LINE)
        );
        assert_eq!(observation.startup_reload_after_startup, None);
        assert!(observation.markers_observed(false));
        assert!(observation.missing_markers(false).is_empty());
        assert!(!observation.markers_observed(true));
        assert_eq!(
            observation.missing_markers(true),
            vec![REDRAW_AFTER_STARTUP_DESCRIPTION]
        );

        let replay = LogReplay::new(REAL_MACOS_INITIAL_TURN, Instant::now());
        // The newest activity line before the paste is the plain reload at
        // 21:32:14.739; one quiet period later the log-only rule is ready, although
        // the dialog was still up when the smoke pasted at 21:32:18.79.
        assert_eq!(
            replay.states(
                MACOS_FOLLOW_UP_READINESS_TIMING,
                &["21:32:14.800000", "21:32:18.790000"]
            ),
            vec![ReadinessState::Settling, ReadinessState::Ready]
        );
        let ready = replay_readiness(&replay, MACOS_FOLLOW_UP_READINESS_TIMING);
        assert_ready_at(
            &replay,
            ready,
            "21:32:18.239083",
            "session-QMFk6F, macOS rule",
        );
        assert!(
            ready < replay.recorded("21:32:19.637013"),
            "the trust reload is the paste's consequence, not something the gate waits for"
        );

        let mut clock = FakeClock::new(replay.start);
        let error = wait_for_startup_readiness_with(
            &mut replay.reader(clock.shared()),
            replay.start + Duration::from_secs(120),
            WINDOWS_STARTUP_READINESS_TIMING,
            REPLAY_POLL,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(REDRAW_AFTER_STARTUP_DESCRIPTION),
            "{message}"
        );
        assert!(message.contains("the prompt was not pasted"), "{message}");
    }

    // A skills reload right after a conversation start is the conversation reload on
    // Windows too, and a reload that follows a conversation start by more than the
    // latency bound is still the deferred one.
    #[test]
    fn conversation_reload_rule_is_bounded_by_its_latency() {
        let startup = glog(
            "21:32:08.459241",
            1,
            "analytics.go:187",
            "CLI startup completed (took 285.555917ms)",
        );
        let started = glog(
            "21:32:11.355776",
            337,
            "conversation_manager.go:512",
            "Starting new conversation (agent=false)",
        );
        let prompt_reload = glog("21:32:11.362782", 423, "manager.go:1331", SKILLS_RELOAD);
        let log = startup.clone() + &started + &prompt_reload;
        let observation = observe_startup(log.as_bytes());
        assert_eq!(observation.conversation_reload_after_startup, Some(2));
        assert_eq!(observation.deferred_reload_after_startup, None);

        let late_reload = glog("21:32:12.400000", 623, "manager.go:1331", SKILLS_RELOAD);
        let log = startup.clone() + &started + &late_reload;
        let observation = observe_startup(log.as_bytes());
        assert_eq!(observation.conversation_reload_after_startup, None);
        assert_eq!(
            observation.deferred_reload_after_startup,
            Some(2),
            "1.04 s after the conversation start is past the bound"
        );

        // A reload stamped before the conversation start (threads out of order) is
        // judged against startup only.
        let early_reload = glog("21:32:11.300000", 623, "manager.go:1331", SKILLS_RELOAD);
        let log = startup + &started + &early_reload;
        let observation = observe_startup(log.as_bytes());
        assert_eq!(observation.conversation_reload_after_startup, None);
        assert_eq!(observation.deferred_reload_after_startup, Some(2));
    }

    // The macOS paste is the verbatim `correlated_prompt`; iTerm2 pastes it as one
    // multi-line record and Agy logs it Go-quoted with `\n` escapes and the complete
    // marker (session-QMFk6F, 21:42:19). A composer line that merely quotes the marker
    // carries neither framing and never confirms delivery.
    #[test]
    fn macos_receipt_matches_the_verbatim_framed_follow_up() {
        let pending = PendingAgyTurn::new("20087-1790253138785843000-0").unwrap();
        let framed = terminal_correlated_prompt(
            "Reply with exactly this marker and nothing else: AGENT_BRIDGE_NATIVE_AGY_RESULT_OK_DETACHED",
            &pending,
            false,
        )
        .unwrap();
        assert!(framed.contains(TURN_PROTOCOL_HEADER));
        assert!(!framed.contains(WINDOWS_PROTOCOL_PREFIX));
        let quoted = go_quoted(&framed);
        assert!(quoted.contains("\\n[Agent Bridge Agy turn protocol]\\n"));
        let pre_paste_len = REAL_MACOS_INITIAL_TURN.len();
        let delivered = REAL_MACOS_INITIAL_TURN.to_owned() + &receipt_line(&quoted);
        assert_eq!(
            observe_input_receipt(Some(delivered.as_bytes()), pre_paste_len, &pending),
            ReceiptEvidence::Delivered
        );
        let typed = REAL_MACOS_INITIAL_TURN.to_owned()
            + &receipt_line(&go_quoted(&format!("please echo {}", pending.marker)));
        assert!(matches!(
            observe_input_receipt(Some(typed.as_bytes()), pre_paste_len, &pending),
            ReceiptEvidence::NoReceipt { .. }
        ));
        // The Windows framing still matches, so a Windows session's receipt is judged
        // by the same rule.
        let windows = REAL_MACOS_INITIAL_TURN.to_owned() + &framed_receipt_line("x", &pending);
        assert_eq!(
            observe_input_receipt(Some(windows.as_bytes()), pre_paste_len, &pending),
            ReceiptEvidence::Delivered
        );
    }

    // The launcher path for the smoke's lost macOS follow-up, which had no trust
    // evidence: with the log as it stood before the paste the log-only gate is ready
    // after one quiet period, the paste lands on the trust dialog, Agy logs the trust
    // reload and no receipt, and the follow-up is recorded delivery-uncertain with the
    // claim retained.
    #[test]
    fn lost_macos_follow_up_paste_ends_delivery_uncertain_with_the_reason_in_status_json() {
        use super::super::super::TURN_CLAIM_FILE;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-QMFk6F");
        fs::create_dir_all(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Ready, None, None).unwrap();
        // A follow-up claims the ready session before it prepares the paste.
        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        let mut claim = acquire_turn_claim(&directory).unwrap();
        let pending = install_pending_turn(&directory, claim.token()).unwrap();

        let before_reload = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
            .rfind("I0924 21:32:19.637013")
            .unwrap()];
        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let pre_paste_len = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(before_reload)]),
            start + Duration::from_secs(300),
            MACOS_FOLLOW_UP_READINESS_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(
            clock.slept, STARTUP_QUIET_PERIOD,
            "a static macOS log waits one quiet period; it cannot show the trust dialog"
        );
        assert_eq!(pre_paste_len, before_reload.len());
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let pasted_at = clock.now();
        let failure = confirm_input_receipt_with(
            &mut log_sequence(vec![
                some_log(before_reload),
                some_log(REAL_MACOS_INITIAL_TURN),
            ]),
            &pending,
            pre_paste_len,
            pasted_at,
            start + Duration::from_secs(300),
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());

        {
            let delivery_error = failure.error();
            let _ = claim.settle_delivery(if failure.delivery_may_have_occurred() {
                turn::Delivery::Uncertain(delivery_error)
            } else {
                turn::Delivery::NotSent(delivery_error)
            });
        };
        drop(claim);

        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "working");
        let error = status
            .error
            .expect("the reason is recorded in status.error");
        assert!(error.contains(&format!(
            "Agy input receipt for turn marker {} was not confirmed",
            pending.marker
        )));
        assert!(error.contains("no HandleUserInput receipt in the"));
        assert!(
            directory.join(TURN_CLAIM_FILE).exists(),
            "the turn claim stays until the caller closes or the target completes"
        );
        assert!(read_pending_turn(&directory).unwrap().is_some());
    }

    // The former 45 s rule on a Windows session that never logged the trust reload
    // (its workspace was trusted before launch): Agy's own stamps prove a window's
    // worth of runtime, so that rule settled the window from the log and pasted after
    // one quiet period instead of waiting the whole window on its own clock. A log
    // whose newest stamp is inside the window still waited on the gate's clock. The
    // rules in force have no window to settle.
    #[test]
    fn a_window_rule_settles_its_window_from_the_logged_runtime() {
        let quota_line = glog(
            "17:21:20.000000",
            248,
            "quota_manager.go:41",
            "doRefreshQuota: skipped (throttled)",
        );
        let aged = REAL_QUIET_STARTUP.to_owned() + &quota_line;
        let observation = observe_startup(aged.as_bytes());
        assert_eq!(observation.deferred_reload_after_startup, None);
        assert_eq!(observation.newest_stamp, glog_timestamp(&quota_line));

        let start = Instant::now();
        let mut clock = FakeClock::new(start);
        let pre_paste_len = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&aged)]),
            start + Duration::from_secs(300),
            ROUND_9_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(pre_paste_len, aged.len());
        assert_eq!(
            clock.slept, STARTUP_QUIET_PERIOD,
            "only the quiet period from the newest activity line, seen at the first read"
        );

        // The same log without the aged line is still inside the window on Agy's
        // clock, so the 45 s rule waited on its own clock.
        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(REAL_QUIET_STARTUP)]),
            start + Duration::from_secs(300),
            ROUND_9_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, DEFERRED_RELOAD_WINDOW);

        // The rules in force pass both logs after one quiet period.
        for log in [aged.as_str(), REAL_QUIET_STARTUP] {
            let mut clock = FakeClock::new(start);
            wait_for_startup_readiness_with(
                &mut log_sequence(vec![some_log(log)]),
                start + Duration::from_secs(300),
                WINDOWS_STARTUP_READINESS_TIMING,
                Duration::from_millis(100),
                &mut clock,
            )
            .unwrap();
            assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
        }

        // The macOS rule passes the un-aged macOS log after one quiet period.
        let before_reload = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
            .rfind("I0924 21:32:19.637013")
            .unwrap()];
        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(before_reload)]),
            start + Duration::from_secs(300),
            MACOS_FOLLOW_UP_READINESS_TIMING,
            Duration::from_millis(100),
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, STARTUP_QUIET_PERIOD);
    }

    // session-C2fMs7 and session-uTpvwY (the Windows machine, 2026-10-01 22:43 KST, Agy
    // 1.2.14, native Windows console in a Windows Terminal tab, the rule without a
    // window): the startup of two sessions in a workspace trusted before launch,
    // verbatim. The omitted lines are HTTP, auth, model and quota chatter. Each logs
    // the workspace customization load at startup (goroutines 327 and 311) and no
    // trust reload afterwards. The pastes followed the quiet period, at 22:43:26.27
    // and 22:43:48.67 (8.8 and 7.9 s after startup), and Agy logged their receipts
    // at 22:43:27.485085 and 22:43:50.112237. This is the LIVE paste right after the
    // quiet period that the 45 s window was kept waiting for; session-jkxi48 (22:38,
    // Agy 1.2.10, the 45 s rule) had logged the same startup, no trust reload, and
    // its receipt 2.1 s after the paste at +45 s.
    const REAL_WINDOWS_TRUSTED_STARTUP: &str = r"I1001 22:43:17.413565       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:17.418295       1 common.go:448] Starting CLI program
CLI ready for user input
I1001 22:43:17.423669     328 manager.go:1333] Reloading system slash commands and skills
I1001 22:43:17.423669     328 manager.go:1310] Reloading system slash commands
I1001 22:43:17.423669     328 manager.go:1314] Slash commands unchanged, skipping update
I1001 22:43:17.426377     327 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:17.426377       1 analytics.go:189] CLI startup completed (took 286.5143ms)
I1001 22:43:17.472179     499 manager.go:935] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I1001 22:43:19.933070     614 manager.go:1310] Reloading system slash commands
I1001 22:43:22.687836     654 manager.go:1310] Reloading system slash commands
I1001 22:43:22.693045     654 manager.go:1314] Slash commands unchanged, skipping update
";
    const REAL_WINDOWS_TRUSTED_STARTUP_AGAIN: &str = r"I1001 22:43:40.797304       1 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:40.802125       1 common.go:448] Starting CLI program
CLI ready for user input
I1001 22:43:40.808780     312 manager.go:1333] Reloading system slash commands and skills
I1001 22:43:40.808780     312 manager.go:1310] Reloading system slash commands
I1001 22:43:40.808780     312 manager.go:1314] Slash commands unchanged, skipping update
I1001 22:43:40.808780       1 analytics.go:189] CLI startup completed (took 281.1586ms)
I1001 22:43:40.808780     311 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I1001 22:43:40.856868     247 manager.go:935] Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)
I1001 22:43:42.633579      55 manager.go:1310] Reloading system slash commands
I1001 22:43:44.991892     448 manager.go:1310] Reloading system slash commands
I1001 22:43:44.994160     448 manager.go:1314] Slash commands unchanged, skipping update
";

    #[test]
    fn a_windows_paste_right_after_the_quiet_period_needs_no_window_in_a_trusted_workspace() {
        for (session, log, ready_at, window_end) in [
            (
                "session-C2fMs7",
                REAL_WINDOWS_TRUSTED_STARTUP,
                "22:43:26.187836",
                "22:44:02.426377",
            ),
            (
                "session-uTpvwY",
                REAL_WINDOWS_TRUSTED_STARTUP_AGAIN,
                "22:43:48.491892",
                "22:44:25.808780",
            ),
        ] {
            assert!(
                workspace_customizations_loaded(log.as_bytes()),
                "{session}: the customization load is logged at startup"
            );
            let observation = observe_startup(log.as_bytes());
            assert!(observation.markers_observed(true), "{session}");
            assert_eq!(
                observation.deferred_reload_after_startup, None,
                "{session}: no trust reload follows the startup of a trusted workspace"
            );
            // Ready 3.5 s after the last plain reload, where the recorded paste was
            // delivered; the former rule would have held it until the window ended.
            let replay = LogReplay::new(log, Instant::now());
            let ready = replay_readiness(&replay, WINDOWS_STARTUP_READINESS_TIMING);
            assert_ready_at(&replay, ready, ready_at, session);
            assert_ready_at(
                &replay,
                replay_readiness(&replay, ROUND_9_TIMING),
                window_end,
                &format!("{session}, the 45 s rule"),
            );
        }
    }

    // Issue #48, reproduced 2026-10-01 with Agy 1.2.14 (session-U2yPxX, macOS, iTerm2):
    // the workspace was not in Agy's trust store, the dialog covered the composer, and
    // the follow-up was pasted onto it. These four lines are everything Agy logged
    // afterwards, verbatim: the trust reload, 356 bytes, and no receipt. That is the
    // exact failure text of the issue, and it can only end delivery-uncertain.
    const REAL_TRUST_RELOAD_AFTER_PASTE: &str = r"I1001 16:29:08.527232     958 manager.go:1333] Reloading system slash commands and skills
I1001 16:29:08.527267     958 manager.go:1310] Reloading system slash commands
I1001 16:29:08.528761     954 hooks_manager.go:53] loaded 1 named hooks from 1 hooks.json file(s)
I1001 16:29:08.529657     958 manager.go:1314] Slash commands unchanged, skipping update
";

    #[test]
    fn a_paste_onto_the_trust_dialog_leaves_only_the_trust_reload_and_no_receipt() {
        let pending = PendingAgyTurn::new("21521-1790839736415840000-1").unwrap();
        let before = settled_startup_log();
        let after = before.clone() + REAL_TRUST_RELOAD_AFTER_PASTE;
        let evidence = observe_input_receipt(Some(after.as_bytes()), before.len(), &pending);
        assert_eq!(
            evidence,
            ReceiptEvidence::NoReceipt {
                appended: 356,
                partial_tail: false
            }
        );
        let message = unconfirmed_receipt_error(
            &evidence,
            &pending,
            before.len(),
            INPUT_RECEIPT_WINDOW,
            true,
        )
        .to_string();
        assert!(
            message.contains("no HandleUserInput receipt in the 356 bytes appended"),
            "{message}"
        );
    }

    // What tells a trusted Agy process from one whose dialog is still open, in the
    // recorded logs of both platforms: a `hooks_manager.go` line from a goroutine
    // other than the main one. The two delivered Windows pastes (session-udT6uY,
    // session-M8QFPp) had it at startup; every lost paste logged it only after the
    // paste's own Enter approved the dialog; session-IQHEwf, which was never
    // approved, never logged it.
    #[test]
    fn workspace_customization_load_is_read_from_the_sessions_own_log() {
        let macos_before_approval = &REAL_MACOS_INITIAL_TURN[..REAL_MACOS_INITIAL_TURN
            .rfind("I0924 21:32:19.637013")
            .unwrap()];
        for (log, loaded, what) in [
            (
                REAL_SUCCESS_STARTUP,
                true,
                "session-udT6uY, trusted at startup",
            ),
            (
                REAL_STARTUP_RELOAD_AFTER_STARTUP,
                true,
                "session-M8QFPp, trusted at startup",
            ),
            (
                REAL_FAILURE_STARTUP,
                false,
                "session-fMqSQc before its paste",
            ),
            (REAL_QUIET_STARTUP, false, "session-IQHEwf, never approved"),
            (
                macos_before_approval,
                false,
                "session-QMFk6F before its paste",
            ),
            (
                REAL_MACOS_INITIAL_TURN,
                true,
                "session-QMFk6F after the paste approved the dialog",
            ),
            (
                REAL_TRUST_RELOAD_AFTER_PASTE,
                true,
                "session-U2yPxX trust reload",
            ),
        ] {
            assert_eq!(
                workspace_customizations_loaded(log.as_bytes()),
                loaded,
                "{what}"
            );
        }
        assert!(
            workspace_customizations_loaded(settled_startup_log().as_bytes()),
            "session-fMqSQc after the paste approved the dialog"
        );
        // The main goroutine's line while the store manager is built is not the load,
        // and a line still being written is not evidence yet.
        let main_only = glog("16:41:07.819578", 1, "hooks_manager.go:53", HOOKS_LOADED);
        assert!(!workspace_customizations_loaded(main_only.as_bytes()));
        let partial = glog("16:41:20.814059", 406, "hooks_manager.go:53", HOOKS_LOADED);
        assert!(!workspace_customizations_loaded(
            partial.trim_end().as_bytes()
        ));
        assert!(workspace_customizations_loaded(partial.as_bytes()));
        // A field that is not a goroutine id is not evidence either.
        let not_an_id = partial.replace("     406 ", "     4o6 ");
        assert!(!workspace_customizations_loaded(not_an_id.as_bytes()));
    }

    // The fix for that state: no paste, before any terminal input, until Agy's own
    // trust store lists the exact workspace and this session's own log shows the
    // workspace customization load. A parent entry does not count, a dialog approved
    // in another session leaves this session's dialog open, a log without the load
    // line is reported as unverified rather than as an open dialog, an unreadable
    // store proves nothing, and an approval during the wait releases the paste.
    #[test]
    fn the_trust_wait_ends_when_the_session_has_ended() {
        let start = Instant::now();
        let wait = Duration::from_secs(60);

        // Issue #60: the launcher of a closed session went on waiting until its
        // deadline. The session is looked at before every look at the trust evidence.
        let mut clock = FakeClock::new(start);
        let mut polls = 0;
        let error = wait_for_workspace_trust_with(
            &mut || Ok(Some(TRUST_STORE_MISSING)),
            &mut || {
                polls += 1;
                (polls == 3).then(|| "closed".to_owned())
            },
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "the session is closed and no longer waits for Agy workspace trust, so the prompt was not pasted"
        );
        assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 2);

        // An ended session is not pasted into even when its trust is evident.
        let mut clock = FakeClock::new(start);
        let mut looked = false;
        assert!(
            wait_for_workspace_trust_with(
                &mut || {
                    looked = true;
                    Ok(None)
                },
                &mut || Some("failed".to_owned()),
                start + wait,
                STARTUP_POLL_INTERVAL,
                &mut clock,
            )
            .is_err()
        );
        assert!(!looked);
        assert_eq!(clock.slept, Duration::ZERO);
    }

    #[test]
    fn a_paste_is_withheld_until_the_store_and_this_sessions_log_show_trust() {
        use super::super::super::consent;
        use super::super::super::doctor::Availability;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let directory = root.join("session-trust1");
        fs::create_dir(&directory).unwrap();
        let log_path = directory.join(AGY_LOG_FILE);
        let homes = consent::fixture_homes(&root);
        let trust_store = |entries: &[&Path]| {
            let keys: Vec<String> = entries
                .iter()
                .map(|path| consent::native_key(path).unwrap())
                .collect();
            super::super::super::write_json_atomic(
                &homes.agy,
                &serde_json::json!({ "trustedWorkspaces": keys }),
            )
            .unwrap();
        };
        let start = Instant::now();
        let wait = Duration::from_secs(30);
        let refused = |expected: &str, availability: Availability, reason_code: &str| {
            let mut clock = FakeClock::new(start);
            let error = wait_for_workspace_trust_with(
                &mut || workspace_trust_missing(&workspace, &homes, &log_path),
                &mut || None,
                start + wait,
                STARTUP_POLL_INTERVAL,
                &mut clock,
            )
            .unwrap_err()
            .to_string();
            assert_eq!(clock.slept, wait, "the wait ends only at the deadline");
            assert_eq!(
                error,
                format!(
                    "Agy workspace trust was not verified before the deadline, so the prompt was not pasted: {expected}. {TRUST_RECOVERY}"
                )
            );
            let check = workspace_trust_check_with(&workspace, &homes, &directory);
            assert_eq!(check.availability, availability);
            assert_eq!(check.reason_code, reason_code);
            let next_action = serde_json::to_value(&check).unwrap()["next_action"].to_string();
            assert!(next_action.contains(TRUST_RECOVERY), "{next_action}");
        };
        let untrusted = || {
            refused(
                TRUST_STORE_MISSING,
                Availability::Unavailable,
                "agy_workspace_untrusted",
            )
        };
        let unverified = || {
            refused(
                TRUST_SESSION_UNVERIFIED,
                Availability::Unknown,
                "agy_session_trust_unverified",
            )
        };
        let passes = |what: &str| {
            let mut clock = FakeClock::new(start);
            wait_for_workspace_trust_with(
                &mut || workspace_trust_missing(&workspace, &homes, &log_path),
                &mut || None,
                start + wait,
                STARTUP_POLL_INTERVAL,
                &mut clock,
            )
            .unwrap();
            assert_eq!(clock.slept, Duration::ZERO, "{what}");
            let check = workspace_trust_check_with(&workspace, &homes, &directory);
            assert_eq!(check.availability, Availability::Available, "{what}");
            assert_eq!(check.reason_code, "agy_workspace_trusted", "{what}");
        };

        // The workspace is not in the store: issue #48. Neither no store nor a
        // parent entry is an approval, whatever the log says.
        fs::write(&log_path, REAL_FAILURE_STARTUP).unwrap();
        untrusted();
        trust_store(&[&root]);
        untrusted();
        fs::write(&log_path, REAL_SUCCESS_STARTUP).unwrap();
        untrusted();

        // The store lists the workspace because another session approved it, and
        // this session's log shows no load: its own dialog may still be open. The
        // same holds for a log that is gone or was cut before the load line, in a
        // session that has no dialog, so the report claims neither state.
        trust_store(&[&root, &workspace]);
        fs::write(&log_path, REAL_FAILURE_STARTUP).unwrap();
        unverified();
        fs::remove_file(&log_path).unwrap();
        unverified();
        let cut = REAL_SUCCESS_STARTUP
            .find("I0924 16:42:24.081497")
            .expect("the load line of session-udT6uY");
        fs::write(&log_path, &REAL_SUCCESS_STARTUP[..cut]).unwrap();
        assert!(REAL_SUCCESS_STARTUP[..cut].contains(STARTUP_COMPLETED_MARKER));
        unverified();

        // Approved in this session, or trusted before it started.
        fs::write(&log_path, settled_startup_log()).unwrap();
        passes("the dialog was approved in this session");
        fs::write(&log_path, REAL_SUCCESS_STARTUP).unwrap();
        passes("the workspace was trusted before launch");

        // An approval during the wait releases the paste at the next poll.
        let mut answers = [
            Ok(Some(TRUST_STORE_MISSING)),
            Err(anyhow::anyhow!("store busy")),
            Ok(Some(TRUST_SESSION_UNVERIFIED)),
            Ok(None),
        ]
        .into_iter();
        let mut clock = FakeClock::new(start);
        wait_for_workspace_trust_with(
            &mut || answers.next().unwrap(),
            &mut || None,
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, STARTUP_POLL_INTERVAL * 3);

        // An unreadable store is not an approval, and the report says why.
        fs::write(&homes.agy, b"{\"trustedWorkspaces\":true}").unwrap();
        let mut clock = FakeClock::new(start);
        let error = wait_for_workspace_trust_with(
            &mut || workspace_trust_missing(&workspace, &homes, &log_path),
            &mut || None,
            start + wait,
            STARTUP_POLL_INTERVAL,
            &mut clock,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("so the prompt was not pasted"), "{error}");
        assert!(error.contains("could not be read"), "{error}");
        assert_eq!(
            workspace_trust_check_with(&workspace, &homes, &directory).availability,
            Availability::Unknown
        );
    }

    #[test]
    fn transcript_cursor_records_each_completed_response_once() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();

        let id = "3e166585-bc21-43b7-b3d1-dec5e67688b3";
        let brain = root.path().join("brain");
        let transcript_path = brain
            .join(id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        let first_pending = claim_pending_turn(&directory);
        let transcript = [
            planner_line(1, &marked("first", &first_pending)),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "source": "MODEL",
                "step_index": 2,
                "content": "still working",
                "tool_calls": [{"name": "run_command"}],
            })
            .to_string(),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "source": "MODEL",
                "step_index": 3,
                "content": "short...",
                "is_truncated": true,
            })
            .to_string(),
        ]
        .join("\n")
            + "\n";
        fs::write(&transcript_path, transcript).unwrap();
        let mut cursor = TranscriptCursor::new(transcript_path.clone());
        cursor.poll(&directory, &brain, id).unwrap();
        cursor.poll(&directory, &brain, id).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);
        assert!(directory.join(PENDING_TURN_FILE).is_file());

        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let second_pending = claim_pending_turn(&directory);
        let full_path = transcript_path.with_file_name("transcript_full.jsonl");
        let results = ResultReader {
            full_path: &full_path,
            brain_root: &brain,
        };
        let truncated = PlannerResult {
            step: 3,
            message: "short...".to_owned(),
            truncated: true,
        };
        assert!(matches!(
            results
                .observe(&truncated, || panic!(
                    "incomplete evidence must not bind a turn"
                ))
                .unwrap(),
            ResultEvidence::Incomplete
        ));
        assert!(
            !results
                .contains(std::slice::from_ref(&transcript_path), &second_pending)
                .unwrap()
        );
        fs::write(
            transcript_path.with_file_name("transcript_full.jsonl"),
            format!(
                "{}\n{}\n",
                planner_line(1, &marked("first", &first_pending)),
                planner_line(3, &marked("complete long response", &second_pending)),
            ),
        )
        .unwrap();
        let before = fs::read(directory.join("status.json")).unwrap();
        assert!(
            results
                .contains(std::slice::from_ref(&transcript_path), &second_pending)
                .unwrap()
        );
        assert_eq!(fs::read(directory.join("status.json")).unwrap(), before);
        assert_eq!(event_paths(&directory).unwrap().len(), 1);
        cursor.poll(&directory, &brain, id).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!(latest.message, "complete long response");

        let mut transcript = OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let third_pending = claim_pending_turn(&directory);
        writeln!(
            transcript,
            "{}",
            planner_line(4, &marked("second", &third_pending))
        )
        .unwrap();
        cursor.poll(&directory, &brain, id).unwrap();

        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 3);
        let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!(latest.message, "second");
        assert_eq!(latest.provider_session_id.as_deref(), Some(id));
        assert_eq!(latest.turn_id.as_deref(), Some("4"));
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
    }

    #[test]
    fn monitor_switches_to_the_newest_created_conversation() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let brain = root.path().join("brain");
        let log = directory.join("agy.log");
        let first_id = "11111111-1111-1111-1111-111111111111";
        let second_id = "22222222-2222-2222-2222-222222222222";
        for id in [first_id, second_id] {
            let transcript = brain
                .join(id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(transcript, "").unwrap();
        }
        let first_pending = claim_pending_turn(&directory);
        fs::write(
            brain
                .join(first_id)
                .join(".system_generated/logs/transcript.jsonl"),
            format!(
                "{}\n",
                planner_line(1, &marked("before clear", &first_pending))
            ),
        )
        .unwrap();
        fs::write(&log, format!("Created conversation {first_id}\n")).unwrap();
        let mut monitor = MonitorState::default();

        monitor.poll(&directory, &log, &brain).unwrap();
        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let second_pending = claim_pending_turn(&directory);
        fs::write(
            brain
                .join(second_id)
                .join(".system_generated/logs/transcript.jsonl"),
            format!(
                "{}\n",
                planner_line(1, &marked("after clear", &second_pending))
            ),
        )
        .unwrap();
        fs::write(
            &log,
            format!("Created conversation {first_id}\n/clear\nCreated conversation {second_id}\n"),
        )
        .unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();

        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let first: SessionEvent = read_json(&paths[0]).unwrap();
        let second: SessionEvent = read_json(&paths[1]).unwrap();
        assert_eq!(first.message, "before clear");
        assert_eq!(first.provider_session_id.as_deref(), Some(first_id));
        assert_eq!(second.message, "after clear");
        assert_eq!(second.provider_session_id.as_deref(), Some(second_id));
        assert_eq!(second.turn_id.as_deref(), Some("1"));
    }

    // Composite fixture: existing real startup/receipt excerpts (2026-09-24),
    // the Forwarding line from the real 2026-10-01 quota fixture, and the exact
    // confirmation text observed once with Agy 1.3.0 on 2026-10-07 in issue #82.
    // This is not a captured continuous 1.3.0 log; no command text was observed.
    const APPROVAL_FORWARDED: &str = "I1001 17:23:48.670594     402 conversation_manager.go:699] Forwarding user message to conversation 97ad12fd-9e7a-4556-83a4-8f0147343657 (items=1, media=0)\n";
    const APPROVAL_LINE: &str = "Surfacing tool confirmation: \"RunCommand\" at step 2\n";

    fn approval_log(pending: &PendingAgyTurn) -> String {
        REAL_QUIET_STARTUP.to_owned()
            + &receipt_line(&go_quoted(
                &terminal_correlated_prompt("reply", pending, false).unwrap(),
            ))
            + REAL_SUCCESS_AFTER_RECEIPT
            + "Created conversation 97ad12fd-9e7a-4556-83a4-8f0147343657\n"
            + APPROVAL_FORWARDED
            + APPROVAL_LINE
    }

    #[test]
    fn tool_confirmation_uses_failure_binding_and_rejects_other_turns() {
        let pending = PendingAgyTurn::new("82-1-0").unwrap();
        let log = approval_log(&pending);
        assert_eq!(
            pending_turn_log(log.as_bytes(), &pending)
                .confirmation
                .as_deref(),
            Some("RunCommand")
        );
        for log in [
            APPROVAL_LINE.to_owned() + APPROVAL_FORWARDED,
            log.clone() + APPROVAL_FORWARDED + APPROVAL_LINE,
            approval_log(&PendingAgyTurn::new("82-2-0").unwrap()),
        ] {
            assert!(
                pending_turn_log(log.as_bytes(), &pending)
                    .confirmation
                    .is_none()
            );
        }
        let failed = log + "agent executor error: quota exhausted\n";
        let observed = pending_turn_log(failed.as_bytes(), &pending);
        assert_eq!(observed.failure.as_deref(), Some("quota exhausted"));
        assert_eq!(
            pending_turn_failure(failed.as_bytes(), &pending),
            Some(("quota exhausted".to_owned(), true))
        );
        for line in [
            "Surfacing tool confirmation: \"\" at step 2",
            "Surfacing tool confirmation: \"RunCommand\" at step x",
            "Surfacing tool confirmation: \"RunCommand\" at step 2 trailing",
        ] {
            assert!(parse_tool_confirmation(line).is_none());
        }
    }

    #[test]
    fn tool_confirmation_is_read_only_and_yields_to_results_and_failures() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-approval");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        let store = Store::open_unchecked(&directory);
        store
            .write_manifest(&super::super::super::SessionManifest {
                schema: 1,
                id: "session-approval".to_owned(),
                provider: "agy".to_owned(),
                provider_path: PathBuf::from("agy"),
                provider_version: "fixture 1.3.0".to_owned(),
                workspace: directory.clone(),
                title: "approval fixture".to_owned(),
                model: None,
                effort: None,
                yolo: false,
                created_unix_ms: 1,
            })
            .unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let pending = claim_pending_turn(&directory);
        let brain = root.path().join("brain");
        let logs = brain.join("97ad12fd-9e7a-4556-83a4-8f0147343657/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let transcript = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        fs::write(&transcript, "").unwrap();
        let log_path = directory.join(AGY_LOG_FILE);
        let log = approval_log(&pending);
        super::super::super::write_private(&log_path, log.as_bytes()).unwrap();
        let before = fs::read(directory.join("status.json")).unwrap();
        let request = super::super::super::requests::for_claim(
            &Reader::open_unchecked(&directory),
            &pending.claim_token,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            pending_tool_confirmation(&directory, &brain).unwrap(),
            Some(("RunCommand".to_owned(), request.request_id))
        );
        assert_eq!(fs::read(directory.join("status.json")).unwrap(), before);
        assert!(event_paths(&directory).unwrap().is_empty());
        assert_eq!(
            turn::current_claim_token(&Reader::open_unchecked(&directory)).unwrap(),
            Some(pending.claim_token.clone())
        );

        // No receipt: the argument-delivered initial turn needs the sole full input.
        let initial = "Created conversation 97ad12fd-9e7a-4556-83a4-8f0147343657\n".to_owned()
            + APPROVAL_FORWARDED
            + APPROVAL_LINE;
        fs::write(&log_path, &initial).unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        let input =
            serde_json::json!({"type":"USER_INPUT", "content": pending.marker}).to_string() + "\n";
        fs::write(&full, &input).unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_some()
        );
        fs::write(&full, input.clone() + &input).unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        fs::write(&log_path, &log).unwrap();
        update_status(
            &directory,
            SessionState::Working,
            None,
            Some("receipt missing".to_owned()),
        )
        .unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        update_status(&directory, SessionState::Working, None, None).unwrap();
        fs::write(
            &log_path,
            log.clone() + "agent executor error: quota exhausted\n",
        )
        .unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        fs::write(&log_path, &log).unwrap();
        fs::write(
            &transcript,
            planner_line(3, &marked("done", &pending)) + "\n",
        )
        .unwrap();
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        // Diagnosis did not consume the result: the ordinary monitor still records it.
        MonitorState::default()
            .poll(&directory, &log_path, &brain)
            .unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);
        assert!(
            pending_tool_confirmation(&directory, &brain)
                .unwrap()
                .is_none()
        );
        let event: SessionEvent = read_json(&event_paths(&directory).unwrap()[0]).unwrap();
        assert_eq!(event.message, "done");
        assert!(event.error.is_none());
    }

    // A turn Agy gives up on is recorded as a failed request at once instead of
    // waiting out the timeout: the quota error of 2026-10-01 (session-UuYk87), with
    // its log lines verbatim. The failing turn must be the pending one by the log's
    // own account: the first, argument-delivered turn whose only transcript input
    // carries the marker, or a pasted turn behind its receipt. An error of a turn
    // typed by hand, of an older turn, or of a first turn that a newer pending claim
    // has not replaced in the log yet is ignored. A result written before the error
    // is seen wins; one written after the failure was recorded is not accepted.
    #[test]
    fn monitor_records_a_turn_agy_gave_up_on_as_a_failed_request() {
        use super::super::super::TURN_CLAIM_FILE;
        const FORWARDED: &str = "I1001 17:23:48.670594     402 conversation_manager.go:699] Forwarding user message to conversation 97ad12fd-9e7a-4556-83a4-8f0147343657 (items=1, media=0)\n";
        const QUOTA: &str = "E1001 17:23:49.333283     246 errorreport.go:224] agent executor error: generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h22m28s.\nE1001 17:23:49.335578     246 errorreport.go:224] generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h22m28s.\n";
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-quota1");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let brain = root.path().join("brain");
        let log = directory.join("agy.log");
        let id = "97ad12fd-9e7a-4556-83a4-8f0147343657";
        let logs = brain.join(id).join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), "").unwrap();
        let full = logs.join("transcript_full.jsonl");
        let user_input = |step: u64, text: &str| {
            serde_json::json!({
                "step_index": step,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "content": format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>"),
            })
            .to_string()
                + "\n"
        };
        let pasted = |pending: &PendingAgyTurn| {
            receipt_line(&go_quoted(
                &terminal_correlated_prompt("again", pending, false).unwrap(),
            ))
        };
        let created =
            format!("I1001 17:23:46.491183     402 server.go:1248] Created conversation {id}\n");
        let first = claim_pending_turn(&directory);
        let mut monitor = MonitorState::default();
        let quota_failure = |event: &SessionEvent| {
            assert_eq!(event.message, "");
            let error = event.error.clone().unwrap();
            assert!(
                error.starts_with("Agy turn failed: generating and executing: RESOURCE_EXHAUSTED (code 429): Individual quota reached."),
                "{error}"
            );
            assert!(error.ends_with("Resets in 2h22m28s."), "{error}");
            assert_eq!(event.provider_session_id.as_deref(), Some(id));
            error
        };

        // The first turn failed, but Agy's record of its input does not carry this
        // claim's marker: nothing is attributed.
        fs::write(&full, user_input(0, "another request")).unwrap();
        fs::write(&log, created.clone() + FORWARDED + QUOTA).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        assert!(event_paths(&directory).unwrap().is_empty());
        assert!(directory.join(TURN_CLAIM_FILE).exists());

        // The argument-delivered first turn is the pending one, and Agy gave up on it.
        fs::write(&full, user_input(0, &marked("request", &first))).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 1, "the failure is recorded once");
        let error = quota_failure(&read_json(&paths[0]).unwrap());
        assert!(
            !directory.join(TURN_CLAIM_FILE).exists(),
            "the failed turn releases its claim"
        );
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state.as_str(), "ready");
        assert_eq!(status.error.as_deref(), Some(error.as_str()));

        // A follow-up is claimed and its input already stands in the transcript, but
        // the log still ends with the first turn's error: that error is not its own.
        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let second = claim_pending_turn(&directory);
        fs::write(
            &full,
            user_input(0, &marked("request", &first)) + &user_input(1, &marked("again", &second)),
        )
        .unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);

        // Its receipt and turn start arrive: still running, nothing to record.
        let running = created + FORWARDED + QUOTA + &pasted(&second) + FORWARDED;
        fs::write(&log, &running).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);
        assert!(directory.join(TURN_CLAIM_FILE).exists());

        // A turn typed by hand after it fails: the newest turn is not the pending one.
        let typed = receipt_line(&go_quoted("typed by hand"));
        fs::write(&log, running.clone() + &typed + FORWARDED + QUOTA).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 1);

        // Agy gives up on the pasted follow-up itself: bound by its receipt.
        let failed = running + QUOTA;
        fs::write(&log, &failed).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let error = quota_failure(&read_json(paths.last().unwrap()).unwrap());
        assert!(!directory.join(TURN_CLAIM_FILE).exists());

        // A result Agy writes after the failure was recorded is not accepted: the
        // failed request released its claim.
        let transcript = logs.join("transcript.jsonl");
        fs::write(
            &transcript,
            planner_line(2, &marked("late", &second)) + "\n",
        )
        .unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        assert_eq!(event_paths(&directory).unwrap().len(), 2);
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.error.as_deref(), Some(error.as_str()));

        // A result that is already written when the error is seen is recorded first.
        update_status(&directory, SessionState::Claimed, None, None).unwrap();
        update_status(&directory, SessionState::Working, None, None).unwrap();
        let third = claim_pending_turn(&directory);
        let mut appended = OpenOptions::new().append(true).open(&transcript).unwrap();
        writeln!(appended, "{}", planner_line(3, &marked("done", &third))).unwrap();
        fs::write(&log, failed + &pasted(&third) + FORWARDED + QUOTA).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        monitor.poll(&directory, &log, &brain).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 3);
        let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!((latest.message.as_str(), latest.error), ("done", None));
    }

    #[cfg(windows)]
    #[test]
    fn windows_brain_root_falls_back_to_userprofile_without_home() {
        assert_eq!(
            default_brain_root(None, Some(std::ffi::OsStr::new(r"C:\Users\agy-user"))).unwrap(),
            PathBuf::from(r"C:\Users\agy-user\.gemini\antigravity-cli\brain")
        );
    }

    #[test]
    fn reopen_is_refused_with_the_adapters_own_reason() {
        let error = ADAPTER
            .verify_reopen_available("5e58ec26-0000-4000-8000-000000000000")
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Agy"),
            "{error}"
        );
        let directory = tempfile::tempdir().unwrap();
        let error = ADAPTER
            .prepare_resume(ResumeContext {
                bridge_executable: std::path::Path::new("/opt/agent-bridge"),
                directory: directory.path(),
                provider_session_id: "5e58ec26-0000-4000-8000-000000000000",
            })
            .unwrap_err();
        assert!(
            error.to_string().starts_with("reopen unsupported: Agy"),
            "{error}"
        );
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }
}
