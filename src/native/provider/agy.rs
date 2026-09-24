use super::{
    CompletionMonitor, CrossSessionMessageContext, CrossSessionMessageFailure,
    CrossSessionMessageResult, FollowUpTransport, InitialPromptTransport, LaunchContext,
    LaunchPlan, NativeProviderAdapter,
};
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

const PENDING_TURN_FILE: &str = "agy-pending-turn.json";

#[derive(Debug, Deserialize, Serialize)]
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

impl NativeProviderAdapter for AgyAdapter {
    fn diagnose(
        &self,
        context: super::super::doctor::Context<'_>,
    ) -> Vec<super::super::doctor::Check> {
        use super::super::doctor::{Availability::Unknown, Check};
        vec![
            Check::new(
                "agy_follow_up",
                Unknown,
                "agy_terminal_fallback",
                "Agy owns terminal-paste follow-up and transcript result monitoring. No verified first-party input path into a running interactive session is integrated.",
                "Inspect the managed owner and terminal. Live input was not tested; retain this fallback until Agy offers a verified native path.",
            ),
            input_receipt_check(context.directory),
        ]
    }

    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let claim_token = super::super::current_turn_claim_token(context.directory)?
            .context("Agy launch has no native turn claim")?;
        let pending = install_pending_turn(context.directory, &claim_token)?;
        let log_path = context.directory.join(AGY_LOG_FILE);
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

    fn initial_prompt_transport(&self) -> InitialPromptTransport {
        if cfg!(windows) {
            InitialPromptTransport::TerminalPasteAfterLaunch
        } else {
            InitialPromptTransport::ProviderArgument
        }
    }

    fn initial_prompt_ready_delay(&self) -> Duration {
        // The Windows console paste waits on Agy's own startup log instead of a
        // fixed delay (see `wait_for_startup_readiness`): the former 12 second
        // delay was shorter than Agy's skills/hooks reload under CPU contention,
        // and a paste that lands before that reload is discarded (issue #43).
        // Non-Windows delivers the initial prompt as an argument and never waits.
        Duration::ZERO
    }

    fn send_initial_prompt(
        &self,
        session: &terminal::TerminalSession,
        prompt_path: &Path,
        deadline: Instant,
    ) -> terminal::TerminalSendResult {
        if cfg!(windows) {
            deliver_windows_console_turn(session, prompt_path, deadline, true)
        } else {
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
            deliver_windows_console_turn(session, prompt_path, deadline, false)
        } else {
            terminal::send_file(session, prompt_path, deadline)
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
    super::super::write_json_atomic(&directory.join(PENDING_TURN_FILE), &pending)?;
    Ok(pending)
}

fn read_pending_turn(directory: &Path) -> Result<Option<PendingAgyTurn>> {
    let Some(text) =
        super::super::read_regular_text_if_present(&directory.join(PENDING_TURN_FILE))?
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
    super::super::remove_file_if_present(&directory.join(PENDING_TURN_FILE))
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

// Windows console delivery verification (Agy adapter fallback, issue #43).
//
// Native Windows Agy has no first-party input API for a running interactive session
// and no ready/accepted signal for a turn, so the adapter pastes the framed prompt
// into the managed console. The TUI silently discards a paste that lands while it is
// still reloading skills and hooks after `CLI startup completed`, and a discarded
// paste leaves the session `working` at an empty composer. Until Agy exposes either a
// first-party input path or a per-turn ready/accepted signal, this adapter reads
// Agy's own `--log-file` output (glog lines) as the only available evidence:
//
// - readiness gate: `CLI startup completed` (analytics.go) and a
//   `Reloading system slash commands and skills` (manager.go) line that is followed
//   by a `hooks_manager.go ... loaded N named hooks` line, then a quiet period without
//   further `Reloading system slash commands`/`Full redraw completed` lines;
// - input receipt: `HandleUserInput called with text: "..."` (input_loop.go) whose
//   text carries the Windows protocol prefix and the pending turn marker.
//
// Delete this section, `initial_prompt_ready_delay`, and the receipt branches of
// `send_initial_prompt`/`send_terminal_follow_up` when Agy provides such a signal
// or an input API; the transcript result monitor is unaffected.
const AGY_LOG_FILE: &str = "agy.log";
const STARTUP_COMPLETED_MARKER: &str = "CLI startup completed";
const SKILLS_RELOAD_MARKER: &str = "Reloading system slash commands and skills";
const SLASH_RELOAD_MARKER: &str = "Reloading system slash commands";
const HOOKS_LOADED_SOURCE: &str = "hooks_manager.go";
const HOOKS_LOADED_MARKER: &str = " named hooks";
const FULL_REDRAW_MARKER: &str = "Full redraw completed";
const INPUT_RECEIPT_MARKER: &str = "HandleUserInput called with text: \"";
const WINDOWS_PROTOCOL_PREFIX: &str = "[Agent Bridge Agy Windows console turn protocol]";
const TURN_MARKER_HEAD: &str = "<!-- agent-bridge-agy-turn:";
// Observed post-login reload bursts arrive about 3.0 seconds apart; the quiet period
// must outlast that cadence so the paste does not land between two of them.
const STARTUP_QUIET_PERIOD: Duration = Duration::from_millis(3500);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INPUT_RECEIPT_WINDOW: Duration = Duration::from_secs(15);
const INPUT_RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn deliver_windows_console_turn(
    session: &terminal::TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
    gate_on_startup: bool,
) -> terminal::TerminalSendResult {
    use terminal::TerminalSendFailure;
    let directory = session_directory_of(session).map_err(TerminalSendFailure::not_sent)?;
    let log_path = directory.join(AGY_LOG_FILE);
    let pending = read_pending_turn(&directory)
        .and_then(|pending| pending.context("Agy turn correlation state is missing"))
        .map_err(TerminalSendFailure::not_sent)?;
    if gate_on_startup {
        wait_for_startup_readiness_until(
            &log_path,
            deadline,
            STARTUP_QUIET_PERIOD,
            STARTUP_POLL_INTERVAL,
        )
        .map_err(TerminalSendFailure::not_sent)?;
    }
    terminal::send_file(session, prompt_path, deadline)?;
    // The composer state after an unconfirmed paste is unknown; never paste again.
    confirm_input_receipt_until(
        &log_path,
        &pending,
        input_receipt_window_end(Instant::now(), deadline),
        INPUT_RECEIPT_POLL_INTERVAL,
    )
}

fn session_directory_of(session: &terminal::TerminalSession) -> Result<PathBuf> {
    let id = session
        .managed_session_id
        .as_deref()
        .context("Agy terminal handle is missing its managed session binding")?;
    super::super::session_directory(id)
}

fn input_receipt_window_end(now: Instant, deadline: Instant) -> Instant {
    now.checked_add(INPUT_RECEIPT_WINDOW)
        .map_or(deadline, |end| end.min(deadline))
}

// Complete log lines only: a line without its terminating newline may still be
// written, so it is neither a marker nor a receipt yet. Terminal escape sequences and
// carriage returns are dropped so a marker is recognised through console noise.
fn complete_log_lines(log: &str) -> impl Iterator<Item = String> + '_ {
    let complete = match log.rfind('\n') {
        Some(end) => &log[..end],
        None => "",
    };
    complete
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(strip_terminal_noise)
}

fn strip_terminal_noise(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for next in characters.by_ref() {
                        if next == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            _ => clean.push(character),
        }
    }
    clean
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StartupObservation {
    startup_completed: bool,
    // A skills reload line that a hooks-loaded line followed. The main thread also
    // logs a hooks-loaded line before the first reload; that earlier line does not
    // count, which is what separates a late reload from a completed one.
    skills_reloaded: bool,
    // Reload and redraw lines seen so far; each new one restarts the quiet period.
    activity_lines: usize,
}

fn observe_startup(log: &str) -> StartupObservation {
    let mut observation = StartupObservation::default();
    let mut reload_awaiting_hooks = false;
    for line in complete_log_lines(log) {
        if line.contains(STARTUP_COMPLETED_MARKER) {
            observation.startup_completed = true;
        }
        if line.contains(SKILLS_RELOAD_MARKER) {
            reload_awaiting_hooks = true;
        } else if reload_awaiting_hooks
            && line.contains(HOOKS_LOADED_SOURCE)
            && line.contains(HOOKS_LOADED_MARKER)
        {
            observation.skills_reloaded = true;
            reload_awaiting_hooks = false;
        }
        if line.contains(SLASH_RELOAD_MARKER) || line.contains(FULL_REDRAW_MARKER) {
            observation.activity_lines += 1;
        }
    }
    observation
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadinessState {
    Ready,
    AwaitingLog,
    AwaitingStartup,
    AwaitingSkillsReload,
    Settling,
}

impl ReadinessState {
    fn describe(self) -> &'static str {
        match self {
            Self::Ready => "startup readiness observed",
            Self::AwaitingLog => "agy.log has not been created",
            Self::AwaitingStartup => "agy.log has no `CLI startup completed` line",
            Self::AwaitingSkillsReload => {
                "agy.log has no completed skills and hooks reload after startup"
            }
            Self::Settling => "agy.log was still reloading or redrawing during the quiet period",
        }
    }
}

struct ReadinessGate {
    quiet_period: Duration,
    activity_lines: usize,
    activity_seen_at: Instant,
}

impl ReadinessGate {
    fn new(now: Instant, quiet_period: Duration) -> Self {
        Self {
            quiet_period,
            activity_lines: 0,
            activity_seen_at: now,
        }
    }

    fn observe(&mut self, log: Option<&str>, now: Instant) -> ReadinessState {
        let Some(log) = log else {
            return ReadinessState::AwaitingLog;
        };
        let observation = observe_startup(log);
        if observation.activity_lines != self.activity_lines {
            self.activity_lines = observation.activity_lines;
            self.activity_seen_at = now;
        }
        if !observation.startup_completed {
            ReadinessState::AwaitingStartup
        } else if !observation.skills_reloaded {
            ReadinessState::AwaitingSkillsReload
        } else if now.saturating_duration_since(self.activity_seen_at) < self.quiet_period {
            ReadinessState::Settling
        } else {
            ReadinessState::Ready
        }
    }
}

fn wait_for_startup_readiness_until(
    log_path: &Path,
    deadline: Instant,
    quiet_period: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let mut gate = ReadinessGate::new(Instant::now(), quiet_period);
    loop {
        let log = super::super::read_regular_text_if_present(log_path)
            .context("Agy startup readiness could not be observed")?;
        let now = Instant::now();
        let state = gate.observe(log.as_deref(), now);
        if state == ReadinessState::Ready {
            return Ok(());
        }
        if now >= deadline {
            bail!(
                "Agy did not report startup readiness before the deadline: {}; the initial prompt was not pasted",
                state.describe()
            );
        }
        thread::sleep(deadline.saturating_duration_since(now).min(poll_interval));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InputReceipt {
    text: String,
    truncated: bool,
}

// Agy logs the accepted input Go-quoted (`%q`): a complete record ends with the
// closing quote. Anything else, or a trailing ellipsis, is treated as truncated so
// a marker cut off by the logger can still be matched by its visible prefix.
fn input_receipts(log: &str) -> Vec<InputReceipt> {
    complete_log_lines(log)
        .filter_map(|line| {
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
        })
        .collect()
}

fn receipt_matches(receipt: &InputReceipt, pending: &PendingAgyTurn) -> bool {
    if !receipt.text.contains(WINDOWS_PROTOCOL_PREFIX) {
        return false;
    }
    if receipt.text.contains(&pending.marker) {
        return true;
    }
    if !receipt.truncated {
        return false;
    }
    // The marker is ASCII, so every byte prefix is a character boundary. Require at
    // least one claim-token character beyond the shared head.
    (TURN_MARKER_HEAD.len() + 1..pending.marker.len())
        .rev()
        .any(|length| receipt.text.ends_with(&pending.marker[..length]))
}

fn find_input_receipt(log: &str, pending: &PendingAgyTurn) -> bool {
    input_receipts(log)
        .iter()
        .any(|receipt| receipt_matches(receipt, pending))
}

fn confirm_input_receipt_until(
    log_path: &Path,
    pending: &PendingAgyTurn,
    window_end: Instant,
    poll_interval: Duration,
) -> terminal::TerminalSendResult {
    use terminal::TerminalSendFailure;
    let started = Instant::now();
    loop {
        let log = match super::super::read_regular_text_if_present(log_path) {
            Ok(log) => log,
            Err(error) => {
                return Err(TerminalSendFailure::delivery_uncertain(error.context(
                    "Agy input receipt could not be verified because agy.log is unreadable; the console paste may have been accepted",
                )));
            }
        };
        if log
            .as_deref()
            .is_some_and(|log| find_input_receipt(log, pending))
        {
            return Ok(());
        }
        let now = Instant::now();
        if now >= window_end {
            let elapsed = now.saturating_duration_since(started).as_secs();
            return Err(match log {
                Some(_) => TerminalSendFailure::not_sent(anyhow::anyhow!(
                    "Agy did not log an input receipt (HandleUserInput) for turn marker {} within {elapsed} seconds after the console paste; the input was not accepted",
                    pending.marker
                )),
                None => TerminalSendFailure::delivery_uncertain(anyhow::anyhow!(
                    "Agy input receipt could not be verified because {} is missing; the console paste may have been accepted",
                    log_path.display()
                )),
            });
        }
        thread::sleep(window_end.saturating_duration_since(now).min(poll_interval));
    }
}

fn input_receipt_check(directory: Option<&Path>) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::Unknown, Check};
    const CHECK_ID: &str = "agy_input_receipt";
    const NEXT_ACTION: &str = "Observation only. Windows console delivery pastes after the startup readiness markers and requires a HandleUserInput receipt for the pending turn marker; a missing receipt is reported as not sent.";
    let Some(directory) = directory else {
        return Check::new(
            CHECK_ID,
            Unknown,
            "agy_log_unavailable",
            "No session was given, so no agy.log startup readiness or input receipt can be observed.",
            NEXT_ACTION,
        );
    };
    let log_path = directory.join(AGY_LOG_FILE);
    let log = match super::super::read_regular_text_if_present(&log_path) {
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
    let pending_received = pending.as_ref().map(|pending| {
        receipts
            .iter()
            .any(|receipt| receipt_matches(receipt, pending))
    });
    let ready = startup.startup_completed && startup.skills_reloaded;
    let (reason, detail) = match (ready, last) {
        (false, _) => (
            "agy_startup_not_ready",
            "agy.log does not yet show startup readiness (CLI startup completed plus a skills and hooks reload).",
        ),
        (true, None) => (
            "agy_no_input_receipt",
            "agy.log shows startup readiness but no HandleUserInput receipt.",
        ),
        (true, Some(_)) => (
            "agy_input_receipt_observed",
            "agy.log shows startup readiness and at least one HandleUserInput receipt; the evidence states whether the last one matches the pending turn marker.",
        ),
    };
    Check::new(CHECK_ID, Unknown, reason, detail, NEXT_ACTION).evidence(serde_json::json!({
        "log": log_path,
        "startup_completed": startup.startup_completed,
        "skills_reloaded": startup.skills_reloaded,
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
                    let _ = super::super::record_provider_monitor_failure(
                        &error_directory,
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
        let full_path = path.with_file_name("transcript_full.jsonl");
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

        let mut file = OpenOptions::new().read(true).open(&self.path)?;
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
            let message = if result.truncated {
                let Some(message) = read_full_result(&self.full_path, brain_root, result.step)?
                else {
                    break;
                };
                message
            } else {
                result.message.clone()
            };
            let step = result.step;
            if let Some(pending) = read_pending_turn(directory)?
                && let Ok(message) = correlated_response(&message, &pending)
            {
                super::super::record_provider_result_for_claim(
                    directory,
                    FirstPartyCli::Agy,
                    message,
                    Some(conversation_id.to_owned()),
                    Some(step.to_string()),
                    Some(&pending.claim_token),
                )
                .context("failed to record the correlated Agy result")?;
            }
            self.greatest_result_step = Some(step);
            self.pending_results.pop_front();
        }
        Ok(())
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

#[derive(Default)]
struct MonitorState {
    conversation_id: Option<String>,
    transcript: Option<TranscriptCursor>,
}

impl MonitorState {
    fn poll(&mut self, directory: &Path, log_path: &Path, brain_root: &Path) -> Result<()> {
        if let Some(log) = super::super::read_regular_text_if_present(log_path)?
            && let Some(newest_id) = parse_conversation_id(&log)
            && self.conversation_id.as_deref() != Some(newest_id.as_str())
        {
            let path = brain_root
                .join(&newest_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            self.conversation_id = Some(newest_id);
            self.transcript = Some(TranscriptCursor::new(path));
        }
        if let (Some(id), Some(cursor)) =
            (self.conversation_id.as_deref(), self.transcript.as_mut())
        {
            cursor.poll(directory, brain_root, id)?;
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
    use std::io::Write;

    fn claim_pending_turn(directory: &Path) -> PendingAgyTurn {
        let claim = acquire_turn_claim(directory).unwrap();
        let token = claim.token.clone();
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

    // Log fixtures follow the glog lines Agy 1.2.10 wrote on 2026-09-24 (issue #43).
    fn glog(time: &str, thread: u32, source: &str, message: &str) -> String {
        format!("I0924 {time} {thread:>7} {source}] {message}\n")
    }

    const HOOKS_LOADED: &str = "loaded 0 named hooks from 0 hooks.json file(s)";
    const SKILLS_RELOAD: &str = "Reloading system slash commands and skills";
    const SLASH_RELOAD: &str = "Reloading system slash commands";
    const FULL_REDRAW: &str =
        "Full redraw completed (rerenderAll) for conversation  (epoch 0, items 1)";

    // session-udT6uY: the startup reload was followed by its hooks reload at once.
    fn successful_startup_log() -> String {
        [
            glog("16:42:24.068931", 1, "hooks_manager.go:53", HOOKS_LOADED),
            glog(
                "16:42:24.080467",
                1,
                "analytics.go:187",
                "CLI startup completed (took 230.2044ms)",
            ),
            glog("16:42:24.080986", 215, "manager.go:1331", SKILLS_RELOAD),
            glog("16:42:24.080986", 215, "manager.go:1308", SLASH_RELOAD),
            glog("16:42:24.081497", 200, "hooks_manager.go:53", HOOKS_LOADED),
            glog("16:42:24.127839", 402, "manager.go:934", FULL_REDRAW),
            glog("16:42:25.848949", 499, "manager.go:1308", SLASH_RELOAD),
        ]
        .concat()
    }

    // session-fMqSQc: the main thread's hooks line precedes the startup reload, the
    // reload itself skipped its hooks pass, and the completed reload came 13 s later.
    fn late_reload_startup_log() -> String {
        [
            glog("16:41:07.819578", 1, "hooks_manager.go:53", HOOKS_LOADED),
            glog(
                "16:41:07.823186",
                1,
                "common.go:438",
                "Starting CLI program",
            ),
            "CLI ready for user input\n".to_owned(),
            glog("16:41:07.826763", 280, "manager.go:1331", SKILLS_RELOAD),
            glog("16:41:07.826763", 280, "manager.go:1308", SLASH_RELOAD),
            glog(
                "16:41:07.826763",
                280,
                "manager.go:1312",
                "Slash commands unchanged, skipping update",
            ),
            glog(
                "16:41:07.830345",
                1,
                "analytics.go:187",
                "CLI startup completed (took 226.5353ms)",
            ),
            glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW),
            glog("16:41:10.732767", 345, "manager.go:1308", SLASH_RELOAD),
            glog("16:41:13.737805", 383, "manager.go:1308", SLASH_RELOAD),
        ]
        .concat()
    }

    fn late_reload_completion() -> String {
        [
            glog("16:41:20.813553", 410, "manager.go:1331", SKILLS_RELOAD),
            glog("16:41:20.813553", 410, "manager.go:1308", SLASH_RELOAD),
            glog("16:41:20.814059", 406, "hooks_manager.go:53", HOOKS_LOADED),
        ]
        .concat()
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

    #[test]
    fn startup_readiness_requires_a_hooks_reload_after_the_skills_reload() {
        let observation = observe_startup(&successful_startup_log());
        assert!(observation.startup_completed);
        assert!(observation.skills_reloaded);
        assert_eq!(observation.activity_lines, 4);

        let late = late_reload_startup_log();
        let observation = observe_startup(&late);
        assert!(observation.startup_completed);
        assert!(
            !observation.skills_reloaded,
            "the hooks line before the reload must not count"
        );
        assert_eq!(observation.activity_lines, 5);

        let completed = late + &late_reload_completion();
        let observation = observe_startup(&completed);
        assert!(observation.skills_reloaded);
        assert_eq!(observation.activity_lines, 7);

        let no_startup = successful_startup_log().replace("CLI startup completed", "CLI startup");
        assert!(!observe_startup(&no_startup).startup_completed);
    }

    #[test]
    fn startup_readiness_ignores_partial_writes_and_terminal_noise() {
        let mut partial = late_reload_startup_log();
        partial.push_str(
            "I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills\nI0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named ho",
        );
        let observation = observe_startup(&partial);
        assert!(!observation.skills_reloaded);
        assert_eq!(observation.activity_lines, 6);
        partial.push_str("oks from 0 hooks.json file(s)\n");
        assert!(observe_startup(&partial).skills_reloaded);

        let noisy = successful_startup_log()
            .lines()
            .map(|line| format!("\u{1b}[32m{line}\u{1b}[0m\r\n"))
            .collect::<String>()
            .replace("CLI startup", "CLI\u{1b}]0;title\u{7} startup");
        let observation = observe_startup(&noisy);
        assert!(observation.startup_completed);
        assert!(observation.skills_reloaded);
        assert_eq!(observation.activity_lines, 4);
        assert_eq!(strip_terminal_noise("a\u{1b}[1;31mb\u{1b}Kc\r"), "abc");
    }

    #[test]
    fn readiness_gate_waits_for_the_quiet_period_after_the_last_reload_or_redraw() {
        let start = Instant::now();
        let quiet = Duration::from_millis(3500);
        let mut gate = ReadinessGate::new(start, quiet);
        assert_eq!(gate.observe(None, start), ReadinessState::AwaitingLog);

        let late = late_reload_startup_log();
        let at = |millis: u64| start + Duration::from_millis(millis);
        assert_eq!(
            gate.observe(Some(&late), at(100)),
            ReadinessState::AwaitingSkillsReload
        );
        let completed = late + &late_reload_completion();
        assert_eq!(
            gate.observe(Some(&completed), at(13_000)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(&completed), at(16_400)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(&completed), at(16_500)),
            ReadinessState::Ready
        );

        let redrawn = completed + &glog("16:41:24.000000", 420, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            gate.observe(Some(&redrawn), at(16_600)),
            ReadinessState::Settling,
            "a new redraw restarts the quiet period"
        );
        assert_eq!(
            gate.observe(Some(&redrawn), at(20_100)),
            ReadinessState::Ready
        );

        let mut immediate = ReadinessGate::new(start, quiet);
        let success = successful_startup_log();
        assert_eq!(
            immediate.observe(Some(&success), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            immediate.observe(Some(&success), at(3_500)),
            ReadinessState::Ready
        );
    }

    #[test]
    fn readiness_gate_fails_as_not_pasted_at_the_deadline() {
        let root = tempfile::tempdir().unwrap();
        let log_path = root.path().join(AGY_LOG_FILE);
        let deadline = Instant::now() + Duration::from_millis(150);
        let error = wait_for_startup_readiness_until(
            &log_path,
            deadline,
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Agy did not report startup readiness before the deadline"));
        assert!(message.contains("agy.log has not been created"));

        fs::write(&log_path, late_reload_startup_log()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(150);
        let error = wait_for_startup_readiness_until(
            &log_path,
            deadline,
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("no completed skills and hooks reload"));

        fs::write(&log_path, successful_startup_log()).unwrap();
        wait_for_startup_readiness_until(
            &log_path,
            Instant::now() + Duration::from_secs(5),
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap();

        fs::create_dir(root.path().join("dir.log")).unwrap();
        let error = wait_for_startup_readiness_until(
            &root.path().join("dir.log"),
            Instant::now() + Duration::from_secs(1),
            Duration::ZERO,
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("startup readiness could not be observed"));
    }

    #[test]
    fn input_receipt_for_a_long_prompt_carries_the_whole_marker_near_the_front() {
        let pending = PendingAgyTurn::new("28404-1790235743098225800-0").unwrap();
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

        let receipts = input_receipts(&line);
        assert_eq!(receipts.len(), 1);
        assert!(!receipts[0].truncated);
        assert_eq!(receipts[0].text, quoted[1..quoted.len() - 1]);
        assert!(receipt_matches(&receipts[0], &pending));
        assert!(find_input_receipt(&line, &pending));

        let other = PendingAgyTurn::new("28404-1790235743098225800-1").unwrap();
        assert!(!find_input_receipt(&line, &other));

        let legacy = format!(
            "ERROR: logging before google.Init: I0827 22:11:28.641211     406 input_loop.go:36] HandleUserInput called with text: {}\n",
            go_quoted(&framed)
        );
        assert!(find_input_receipt(&legacy, &pending));

        let manual = receipt_line(&go_quoted(&format!("please finish {}", pending.marker)));
        assert!(
            !find_input_receipt(&manual, &pending),
            "a manual turn without the protocol prefix is not this delivery"
        );
    }

    #[test]
    fn input_receipt_matches_a_truncated_line_only_through_the_visible_marker_prefix() {
        let pending = PendingAgyTurn::new("28404-1790235743098225800-0").unwrap();
        let framed = terminal_correlated_prompt("short", &pending, true).unwrap();
        let quoted = go_quoted(&framed);
        let marker_at = quoted.find(&pending.marker).unwrap();
        let cut = marker_at + TURN_MARKER_HEAD.len() + 6;

        let truncated_with_ellipsis = receipt_line(&format!("{}...\"", &quoted[..cut]));
        let receipts = input_receipts(&truncated_with_ellipsis);
        assert!(receipts[0].truncated);
        assert!(receipt_matches(&receipts[0], &pending));

        let torn = receipt_line(&quoted[..cut]);
        let receipts = input_receipts(&torn);
        assert!(receipts[0].truncated);
        assert!(receipt_matches(&receipts[0], &pending));

        let before_token = receipt_line(&quoted[..marker_at + TURN_MARKER_HEAD.len()]);
        assert!(!find_input_receipt(&before_token, &pending));
        let before_marker = receipt_line(&format!("{}...\"", &quoted[..marker_at - 1]));
        assert!(!find_input_receipt(&before_marker, &pending));

        let complete_but_different = receipt_line(&quoted.replace("-0 -->", "-7 -->"));
        assert!(!find_input_receipt(&complete_but_different, &pending));

        let mut partial_write = String::from(&truncated_with_ellipsis);
        partial_write.truncate(partial_write.len() - 1);
        assert!(input_receipts(&partial_write).is_empty());
    }

    #[test]
    fn missing_receipt_is_not_sent_but_an_unreadable_log_stays_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let pending = PendingAgyTurn::new("1-2-3").unwrap();
        let framed = terminal_correlated_prompt("hello", &pending, true).unwrap();
        let log_path = root.path().join(AGY_LOG_FILE);
        let poll = Duration::from_millis(10);

        fs::write(&log_path, successful_startup_log()).unwrap();
        let failure = confirm_input_receipt_until(
            &log_path,
            &pending,
            Instant::now() + Duration::from_millis(120),
            poll,
        )
        .unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        let message = format!("{:#}", failure.error());
        assert!(message.contains("did not log an input receipt"));
        assert!(message.contains(&pending.marker));

        let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
        write!(log, "{}", receipt_line(&go_quoted(&framed))).unwrap();
        drop(log);
        confirm_input_receipt_until(&log_path, &pending, Instant::now(), poll).unwrap();

        let missing = root.path().join("absent.log");
        let failure = confirm_input_receipt_until(
            &missing,
            &pending,
            Instant::now() + Duration::from_millis(60),
            poll,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        assert!(format!("{:#}", failure.error()).contains("is missing"));

        let unreadable = root.path().join("dir.log");
        fs::create_dir(&unreadable).unwrap();
        let failure = confirm_input_receipt_until(
            &unreadable,
            &pending,
            Instant::now() + Duration::from_secs(5),
            poll,
        )
        .unwrap_err();
        assert!(failure.delivery_may_have_occurred());
        assert!(format!("{:#}", failure.error()).contains("agy.log is unreadable"));

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
    fn doctor_reports_startup_readiness_and_the_pending_receipt_without_recovery() {
        use super::super::super::doctor::Availability;
        let check = input_receipt_check(None);
        assert_eq!(check.id, "agy_input_receipt");
        assert_eq!(check.availability, Availability::Unknown);
        assert_eq!(check.reason_code, "agy_log_unavailable");

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-doctor1");
        fs::create_dir_all(directory.join("events")).unwrap();
        update_status(&directory, "working", None, None).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_log_missing"
        );

        let log_path = directory.join(AGY_LOG_FILE);
        fs::write(&log_path, late_reload_startup_log()).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_startup_not_ready"
        );

        fs::write(&log_path, successful_startup_log()).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_no_input_receipt"
        );

        let pending = claim_pending_turn(&directory);
        let framed = terminal_correlated_prompt("hello", &pending, true).unwrap();
        let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
        write!(log, "{}", receipt_line(&go_quoted(&framed))).unwrap();
        drop(log);
        let check = input_receipt_check(Some(&directory));
        assert_eq!(check.reason_code, "agy_input_receipt_observed");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["startup_completed"], true);
        assert_eq!(evidence["skills_reloaded"], true);
        assert_eq!(evidence["input_receipts"], 1);
        assert_eq!(evidence["pending_marker_received"], true);
        assert_eq!(evidence["pending_marker"], pending.marker);
        let status: SessionStatus = read_json(&directory.join("status.json")).unwrap();
        assert_eq!(status.state, "working");
    }

    #[test]
    fn transcript_cursor_records_each_completed_response_once() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, "working", None, None).unwrap();

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

        update_status(&directory, "claimed", None, None).unwrap();
        update_status(&directory, "working", None, None).unwrap();
        let second_pending = claim_pending_turn(&directory);
        fs::write(
            transcript_path.with_file_name("transcript_full.jsonl"),
            format!(
                "{}\n{}\n",
                planner_line(1, &marked("first", &first_pending)),
                planner_line(3, &marked("complete long response", &second_pending)),
            ),
        )
        .unwrap();
        cursor.poll(&directory, &brain, id).unwrap();
        let paths = event_paths(&directory).unwrap();
        assert_eq!(paths.len(), 2);
        let latest: SessionEvent = read_json(paths.last().unwrap()).unwrap();
        assert_eq!(latest.message, "complete long response");

        let mut transcript = OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        update_status(&directory, "claimed", None, None).unwrap();
        update_status(&directory, "working", None, None).unwrap();
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
        assert_eq!(status.state, "ready");
    }

    #[test]
    fn monitor_switches_to_the_newest_created_conversation() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-safe123");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("events")).unwrap();
        update_status(&directory, "working", None, None).unwrap();
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
        update_status(&directory, "claimed", None, None).unwrap();
        update_status(&directory, "working", None, None).unwrap();
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

    #[cfg(windows)]
    #[test]
    fn windows_brain_root_falls_back_to_userprofile_without_home() {
        assert_eq!(
            default_brain_root(None, Some(std::ffi::OsStr::new(r"C:\Users\agy-user"))).unwrap(),
            PathBuf::from(r"C:\Users\agy-user\.gemini\antigravity-cli\brain")
        );
    }
}
