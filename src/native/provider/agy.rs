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
// - readiness gate: `CLI startup completed` (analytics.go) and the latest
//   `Reloading system slash commands and skills` (manager.go) line followed by its
//   `hooks_manager.go ... loaded N named hooks` completion, then a quiet period that
//   starts at that completion and restarts on every later
//   `Reloading system slash commands`/`Full redraw completed` line. A later reload
//   without its completion resets readiness. An Agy version that never logs the
//   hooks completion after its skills reload is unsupported by this gate.
// - input receipt: a complete `HandleUserInput called with text: "..."` line
//   (input_loop.go) that starts after the byte length of agy.log observed
//   immediately before the paste and whose text carries the Windows protocol prefix
//   and the complete pending turn marker.
//
// Delivery classification after a paste (issue #43 review):
//
// - delivered: such a receipt line exists after the pre-paste offset;
// - delivery-uncertain: everything else. Non-delivery would have to be proven by a
//   line Agy logs after draining its console input without a receipt, and the real
//   logs contain no such marker (session-fMqSQc, 2026-09-24: after the discarded
//   paste Agy logged only its late reload and then nothing for a minute), so a
//   missing receipt at the end of the window, a deadline-capped window, an
//   unreadable or missing log, a log shorter than the pre-paste offset (rotated or
//   truncated), and a partial trailing line all stay uncertain and never `not_sent`.
//   The paste is never repeated.
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
const HOOKS_COMPLETION_DESCRIPTION: &str = "`hooks_manager.go ... named hooks` after the latest `Reloading system slash commands and skills`";
const FULL_REDRAW_MARKER: &str = "Full redraw completed";
const INPUT_RECEIPT_MARKER: &str = "HandleUserInput called with text: \"";
const WINDOWS_PROTOCOL_PREFIX: &str = "[Agent Bridge Agy Windows console turn protocol]";
// Observed post-login reload bursts arrive about 3.0 seconds apart; the quiet period
// must outlast that cadence so the paste does not land between two of them.
const STARTUP_QUIET_PERIOD: Duration = Duration::from_millis(3500);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INPUT_RECEIPT_WINDOW: Duration = Duration::from_secs(15);
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
    super::super::read_regular_bytes_if_present(log_path)
}

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
        wait_for_startup_readiness_with(
            &mut || read_log_bytes(&log_path),
            deadline,
            STARTUP_QUIET_PERIOD,
            STARTUP_POLL_INTERVAL,
            &mut SystemClock,
        )
        .map_err(TerminalSendFailure::not_sent)?;
    }
    // The paste is issued only after this read, so only lines that start after this
    // offset can be evidence for this submission.
    let pre_paste_len = read_log_bytes(&log_path)
        .context("Agy log could not be read before the console paste")
        .map_err(TerminalSendFailure::not_sent)?
        .map_or(0, |log| log.len());
    terminal::send_file(session, prompt_path, deadline)?;
    let pasted_at = Instant::now();
    // The composer state after an unconfirmed paste is unknown; never paste again.
    confirm_input_receipt_with(
        &mut || read_log_bytes(&log_path),
        &pending,
        pre_paste_len,
        pasted_at,
        deadline,
        INPUT_RECEIPT_POLL_INTERVAL,
        &mut SystemClock,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StartupObservation {
    startup_completed: bool,
    // Line index of the latest `Reloading system slash commands and skills` line.
    latest_reload: Option<usize>,
    // Line index of the hooks-loaded line that followed the latest reload. The main
    // thread also logs a hooks-loaded line before the first reload; that earlier
    // line does not count, which is what separates a late reload from a completed
    // one, and a later reload without its own completion clears this again.
    latest_reload_completion: Option<usize>,
    // Line index of the newest settling event: the latest reload's completion or a
    // reload/redraw line after it. The quiet period starts when this changes.
    settle_line: Option<usize>,
    // Reload and redraw lines seen so far (diagnostics only).
    activity_lines: usize,
}

impl StartupObservation {
    fn missing_markers(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.startup_completed {
            missing.push("`CLI startup completed`");
        }
        if self.latest_reload.is_none() {
            missing.push("`Reloading system slash commands and skills`");
        }
        if self.latest_reload_completion.is_none() {
            missing.push(HOOKS_COMPLETION_DESCRIPTION);
        }
        missing
    }
}

fn observe_startup(log: &[u8]) -> StartupObservation {
    let mut observation = StartupObservation::default();
    for (index, line) in complete_log_lines(log).enumerate() {
        if line.contains(STARTUP_COMPLETED_MARKER) {
            observation.startup_completed = true;
        }
        if line.contains(SKILLS_RELOAD_MARKER) {
            observation.latest_reload = Some(index);
            observation.latest_reload_completion = None;
            observation.settle_line = None;
        } else if observation.latest_reload.is_some()
            && observation.latest_reload_completion.is_none()
            && line.contains(HOOKS_LOADED_SOURCE)
            && line.contains(HOOKS_LOADED_MARKER)
        {
            observation.latest_reload_completion = Some(index);
            observation.settle_line = Some(index);
        }
        if line.contains(SLASH_RELOAD_MARKER) || line.contains(FULL_REDRAW_MARKER) {
            observation.activity_lines += 1;
            if observation.latest_reload_completion.is_some() {
                observation.settle_line = Some(index);
            }
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
    AwaitingHooksCompletion,
    Settling,
}

impl ReadinessState {
    fn describe(self) -> &'static str {
        match self {
            Self::Ready => "startup readiness observed",
            Self::AwaitingLog => "agy.log has not been created",
            Self::AwaitingStartup => "agy.log has no `CLI startup completed` line",
            Self::AwaitingSkillsReload => {
                "agy.log has no `Reloading system slash commands and skills` line"
            }
            Self::AwaitingHooksCompletion => {
                "the latest skills reload in agy.log has no hooks completion after it"
            }
            Self::Settling => {
                "agy.log was still reloading or redrawing during the quiet period after the hooks completion"
            }
        }
    }
}

struct ReadinessGate {
    quiet_period: Duration,
    settle_line: Option<usize>,
    settled_at: Instant,
    last_observation: Option<StartupObservation>,
}

impl ReadinessGate {
    fn new(now: Instant, quiet_period: Duration) -> Self {
        Self {
            quiet_period,
            settle_line: None,
            settled_at: now,
            last_observation: None,
        }
    }

    fn observe(&mut self, log: Option<&[u8]>, now: Instant) -> ReadinessState {
        let Some(log) = log else {
            self.last_observation = None;
            return ReadinessState::AwaitingLog;
        };
        let observation = observe_startup(log);
        if observation.settle_line != self.settle_line {
            self.settle_line = observation.settle_line;
            self.settled_at = now;
        }
        let state = if !observation.startup_completed {
            ReadinessState::AwaitingStartup
        } else if observation.latest_reload.is_none() {
            ReadinessState::AwaitingSkillsReload
        } else if observation.latest_reload_completion.is_none() {
            ReadinessState::AwaitingHooksCompletion
        } else if now.saturating_duration_since(self.settled_at) < self.quiet_period {
            ReadinessState::Settling
        } else {
            ReadinessState::Ready
        };
        self.last_observation = Some(observation);
        state
    }

    fn deadline_report(&self, state: ReadinessState) -> String {
        let missing = match &self.last_observation {
            Some(observation) => observation.missing_markers(),
            None => StartupObservation::default().missing_markers(),
        };
        let missing = if missing.is_empty() {
            "none".to_owned()
        } else {
            missing.join(", ")
        };
        format!(
            "Agy did not report startup readiness before the deadline: {}; missing markers: {missing}; an Agy version that never logs the {HOOKS_COMPLETION_DESCRIPTION} completion is unsupported by this gate; the initial prompt was not pasted",
            state.describe()
        )
    }
}

fn wait_for_startup_readiness_with<L, C>(
    read_log: &mut L,
    deadline: Instant,
    quiet_period: Duration,
    poll_interval: Duration,
    clock: &mut C,
) -> Result<()>
where
    L: FnMut() -> Result<Option<Vec<u8>>>,
    C: Clock,
{
    let mut gate = ReadinessGate::new(clock.now(), quiet_period);
    loop {
        let log = read_log().context("Agy startup readiness could not be observed")?;
        let now = clock.now();
        let state = gate.observe(log.as_deref(), now);
        if state == ReadinessState::Ready {
            return Ok(());
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
fn receipt_matches(receipt: &InputReceipt, pending: &PendingAgyTurn) -> bool {
    receipt.text.contains(WINDOWS_PROTOCOL_PREFIX) && receipt.text.contains(&pending.marker)
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

fn input_receipt_check(directory: Option<&Path>) -> super::super::doctor::Check {
    use super::super::doctor::{Availability::Unknown, Check};
    const CHECK_ID: &str = "agy_input_receipt";
    const NEXT_ACTION: &str = "Observation only. Windows console delivery pastes after the startup readiness markers and requires a HandleUserInput receipt carrying the complete pending turn marker; a missing receipt leaves delivery uncertain and the paste is never repeated.";
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
    let ready = startup.startup_completed && startup.latest_reload_completion.is_some();
    let (reason, detail) = match (ready, last) {
        (false, _) => (
            "agy_startup_not_ready",
            "agy.log does not yet show startup readiness (CLI startup completed plus a skills reload with its hooks completion).",
        ),
        (true, None) => (
            "agy_no_input_receipt",
            "agy.log shows startup readiness but no HandleUserInput receipt.",
        ),
        (true, Some(_)) => (
            "agy_input_receipt_observed",
            "agy.log shows startup readiness and at least one HandleUserInput receipt; the evidence states whether any receipt carries the complete pending turn marker.",
        ),
    };
    Check::new(CHECK_ID, Unknown, reason, detail, NEXT_ACTION).evidence(serde_json::json!({
        "log": log_path,
        "startup_completed": startup.startup_completed,
        "skills_reload_observed": startup.latest_reload.is_some(),
        "latest_reload_completed": startup.latest_reload_completion.is_some(),
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

    // Real log excerpts written by Agy 1.2.10 on 2026-09-24 (issue #43), taken from
    // `%USERPROFILE%\.agent-bridge\native-sessions\<session>\agy.log`. Every kept line
    // is verbatim; the listed line numbers are the file positions and the omitted lines
    // are HTTP, auth, model and quota chatter (the auth lines carry the account email
    // and are not reproduced). Timestamps are local time (KST).
    //
    // session-udT6uY (initial paste delivered): lines 91, 100, 101, 119-123, 134, 147
    // and 153 are the startup. The main thread's hooks line (91) precedes the startup
    // reload (120), whose own hooks completion (122) follows at once.
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
    // 150 and 151. The startup reload (114) skipped its hooks pass, so the only hooks
    // line before 16:41:20 is the main thread's (95). The fixed 12 second delay pasted
    // at about 16:41:19.
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

    // session-fMqSQc lines 153-156, the end of the file: the reload that discarded the
    // paste, 13 s after startup, and the first hooks completion after a skills reload.
    // Nothing was logged after line 156 until the session was closed at 16:42:22: no
    // HandleUserInput receipt and no line that shows the console input was drained.
    const REAL_FAILURE_LATE_RELOAD: &str = r"I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills
I0924 16:41:20.813553     410 manager.go:1308] Reloading system slash commands
I0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named hooks from 0 hooks.json file(s)
I0924 16:41:20.816140     410 manager.go:1312] Slash commands unchanged, skipping update
";

    const REAL_SUCCESS_TOKEN: &str = "28404-1790235743098225800-0";

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
        now: Instant,
        slept: Duration,
    }

    impl FakeClock {
        fn new(now: Instant) -> Self {
            Self {
                now,
                slept: Duration::ZERO,
            }
        }
    }

    impl Clock for FakeClock {
        fn now(&mut self) -> Instant {
            self.now
        }

        fn sleep(&mut self, duration: Duration) {
            self.now += duration;
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
    fn startup_readiness_requires_a_hooks_completion_after_the_latest_skills_reload() {
        let observation = observe_startup(successful_startup_log().as_bytes());
        assert!(observation.startup_completed);
        assert_eq!(observation.latest_reload, Some(4));
        assert_eq!(
            observation.latest_reload_completion,
            Some(6),
            "the main thread's hooks line before the reload must not count"
        );
        assert_eq!(observation.settle_line, Some(10));
        assert_eq!(observation.activity_lines, 6);

        let late = late_reload_startup_log();
        let observation = observe_startup(late.as_bytes());
        assert!(observation.startup_completed);
        assert_eq!(observation.latest_reload, Some(3));
        assert_eq!(observation.latest_reload_completion, None);
        assert_eq!(observation.settle_line, None);
        assert_eq!(observation.activity_lines, 5);

        let completed = late + &late_reload_completion();
        let observation = observe_startup(completed.as_bytes());
        assert_eq!(observation.latest_reload, Some(11));
        assert_eq!(observation.latest_reload_completion, Some(13));
        assert_eq!(observation.settle_line, Some(13));
        assert_eq!(observation.activity_lines, 7);

        // A later reload without its own completion clears readiness again.
        let reloading_again = successful_startup_log()
            + &glog("16:42:50.228104", 580, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:42:50.228760", 580, "manager.go:1308", SLASH_RELOAD);
        let observation = observe_startup(reloading_again.as_bytes());
        assert_eq!(observation.latest_reload, Some(11));
        assert_eq!(observation.latest_reload_completion, None);
        assert_eq!(observation.settle_line, None);
        let completed_again =
            reloading_again + &glog("16:42:50.229000", 581, "hooks_manager.go:53", HOOKS_LOADED);
        let observation = observe_startup(completed_again.as_bytes());
        assert_eq!(observation.latest_reload_completion, Some(13));
        assert_eq!(observation.settle_line, Some(13));

        let no_startup = successful_startup_log().replace("CLI startup completed", "CLI startup");
        assert!(!observe_startup(no_startup.as_bytes()).startup_completed);
        assert_eq!(
            StartupObservation::default().missing_markers(),
            vec![
                "`CLI startup completed`",
                "`Reloading system slash commands and skills`",
                HOOKS_COMPLETION_DESCRIPTION,
            ]
        );
    }

    #[test]
    fn startup_readiness_ignores_partial_writes_and_terminal_noise() {
        let mut partial = late_reload_startup_log();
        partial.push_str(
            "I0924 16:41:20.813553     410 manager.go:1331] Reloading system slash commands and skills\nI0924 16:41:20.814059     406 hooks_manager.go:53] loaded 0 named ho",
        );
        let observation = observe_startup(partial.as_bytes());
        assert_eq!(observation.latest_reload_completion, None);
        assert_eq!(observation.activity_lines, 6);
        partial.push_str("oks from 0 hooks.json file(s)\n");
        assert!(
            observe_startup(partial.as_bytes())
                .latest_reload_completion
                .is_some()
        );

        let noisy = successful_startup_log()
            .lines()
            .map(|line| format!("\u{1b}[32m{line}\u{1b}[0m\r\n"))
            .collect::<String>()
            .replace("CLI startup", "CLI\u{1b}]0;title\u{7} startup")
            .replace("and skills", "and\u{1b}]8;;file:///x\u{1b}\\ skills");
        let observation = observe_startup(noisy.as_bytes());
        assert!(observation.startup_completed);
        assert!(observation.latest_reload_completion.is_some());
        assert_eq!(observation.activity_lines, 6);
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
    fn readiness_gate_starts_the_quiet_period_at_the_hooks_completion() {
        let start = Instant::now();
        let quiet = Duration::from_millis(3500);
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut gate = ReadinessGate::new(start, quiet);
        assert_eq!(gate.observe(None, start), ReadinessState::AwaitingLog);

        let late = late_reload_startup_log();
        assert_eq!(
            gate.observe(Some(late.as_bytes()), at(100)),
            ReadinessState::AwaitingHooksCompletion
        );
        let completed = late + &late_reload_completion();
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(13_000)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(16_400)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(16_500)),
            ReadinessState::Ready
        );

        let redrawn = completed + &glog("16:41:24.000000", 420, "manager.go:934", FULL_REDRAW);
        assert_eq!(
            gate.observe(Some(redrawn.as_bytes()), at(16_600)),
            ReadinessState::Settling,
            "a redraw after the completion restarts the quiet period"
        );
        assert_eq!(
            gate.observe(Some(redrawn.as_bytes()), at(20_100)),
            ReadinessState::Ready
        );

        let mut immediate = ReadinessGate::new(start, quiet);
        let success = successful_startup_log();
        assert_eq!(
            immediate.observe(Some(success.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            immediate.observe(Some(success.as_bytes()), at(3_500)),
            ReadinessState::Ready
        );

        // Delayed hooks: reload and redraw activity long ago does not make readiness
        // immediate once the completion finally appears.
        let mut delayed = ReadinessGate::new(start, quiet);
        let without_hooks = [
            glog(
                "16:41:07.830345",
                1,
                "analytics.go:187",
                "CLI startup completed (took 1ms)",
            ),
            glog("16:41:07.840000", 280, "manager.go:1331", SKILLS_RELOAD),
            glog("16:41:07.840000", 280, "manager.go:1308", SLASH_RELOAD),
            glog("16:41:07.876154", 269, "manager.go:934", FULL_REDRAW),
        ]
        .concat();
        assert_eq!(
            delayed.observe(Some(without_hooks.as_bytes()), at(0)),
            ReadinessState::AwaitingHooksCompletion
        );
        assert_eq!(
            delayed.observe(Some(without_hooks.as_bytes()), at(8_000)),
            ReadinessState::AwaitingHooksCompletion
        );
        let hooks_late =
            without_hooks + &glog("16:41:15.840000", 300, "hooks_manager.go:53", HOOKS_LOADED);
        assert_eq!(
            delayed.observe(Some(hooks_late.as_bytes()), at(8_000)),
            ReadinessState::Settling
        );
        assert_eq!(
            delayed.observe(Some(hooks_late.as_bytes()), at(11_400)),
            ReadinessState::Settling
        );
        assert_eq!(
            delayed.observe(Some(hooks_late.as_bytes()), at(11_500)),
            ReadinessState::Ready
        );
    }

    #[test]
    fn readiness_gate_resets_on_a_later_reload_without_its_hooks_completion() {
        let start = Instant::now();
        let quiet = Duration::from_millis(3500);
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut gate = ReadinessGate::new(start, quiet);
        let success = successful_startup_log();
        assert_eq!(
            gate.observe(Some(success.as_bytes()), at(0)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(success.as_bytes()), at(3_500)),
            ReadinessState::Ready
        );

        let reloading = success.clone()
            + &glog("16:42:29.500000", 600, "manager.go:1331", SKILLS_RELOAD)
            + &glog("16:42:29.500000", 600, "manager.go:1308", SLASH_RELOAD);
        assert_eq!(
            gate.observe(Some(reloading.as_bytes()), at(3_600)),
            ReadinessState::AwaitingHooksCompletion
        );
        assert_eq!(
            gate.observe(Some(reloading.as_bytes()), at(10_000)),
            ReadinessState::AwaitingHooksCompletion,
            "silence after an incomplete reload is not readiness"
        );

        let completed =
            reloading + &glog("16:42:36.000000", 601, "hooks_manager.go:53", HOOKS_LOADED);
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(10_100)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(13_500)),
            ReadinessState::Settling
        );
        assert_eq!(
            gate.observe(Some(completed.as_bytes()), at(13_600)),
            ReadinessState::Ready
        );
    }

    #[test]
    fn readiness_gate_deadline_report_names_every_missing_marker() {
        let start = Instant::now();
        let poll = Duration::from_millis(100);
        let deadline = start + Duration::from_secs(1);
        let compatibility = "unsupported by this gate";

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![Ok(None)]),
            deadline,
            Duration::ZERO,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Agy did not report startup readiness before the deadline"));
        assert!(message.contains("agy.log has not been created"));
        assert!(message.contains("`CLI startup completed`"));
        assert!(message.contains("`Reloading system slash commands and skills`"));
        assert!(message.contains(HOOKS_COMPLETION_DESCRIPTION));
        assert!(message.contains(compatibility));
        assert!(message.contains("the initial prompt was not pasted"));
        assert_eq!(clock.slept, Duration::from_secs(1));

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&late_reload_startup_log())]),
            deadline,
            Duration::ZERO,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("no hooks completion after it"));
        assert!(!message.contains("missing markers: `CLI startup completed`"));
        assert!(message.contains(&format!("missing markers: {HOOKS_COMPLETION_DESCRIPTION}")));
        assert!(message.contains(compatibility));

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
            Duration::ZERO,
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("no `Reloading system slash commands and skills` line"));
        assert!(message.contains(&format!(
            "missing markers: `Reloading system slash commands and skills`, {HOOKS_COMPLETION_DESCRIPTION}"
        )));

        let mut clock = FakeClock::new(start);
        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![some_log(&successful_startup_log())]),
            deadline,
            Duration::from_millis(3500),
            poll,
            &mut clock,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("still reloading or redrawing during the quiet period"));
        assert!(message.contains("missing markers: none"));

        let mut clock = FakeClock::new(start);
        wait_for_startup_readiness_with(
            &mut log_sequence(vec![
                some_log(&late_reload_startup_log()),
                some_log(&(late_reload_startup_log() + &late_reload_completion())),
            ]),
            start + Duration::from_secs(30),
            Duration::from_millis(3500),
            poll,
            &mut clock,
        )
        .unwrap();
        assert_eq!(clock.slept, Duration::from_millis(3600));

        let error = wait_for_startup_readiness_with(
            &mut log_sequence(vec![Err(anyhow::anyhow!(
                "refusing non-regular session file"
            ))]),
            deadline,
            Duration::ZERO,
            poll,
            &mut FakeClock::new(start),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("startup readiness could not be observed"));
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
        let deadline = pasted_at + Duration::from_secs(60);
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
        let deadline = pasted_at + Duration::from_secs(60);
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
        assert!(message.contains("within 15 seconds"));
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

        // A receipt appended between the last read and the deadline check is not
        // seen; the outcome is still uncertain, never not_sent.
        let mut reads = 0;
        let receipt_after_last_read = {
            let startup = startup.clone();
            let receipt = receipt.clone();
            move || {
                reads += 1;
                if reads <= 4 {
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

        // A deadline-capped window that ends before 15 s elapsed.
        let (message, clock) = uncertain(
            vec![some_log(&startup)],
            startup.len(),
            pasted_at + Duration::from_secs(3),
        );
        assert!(
            message
                .contains("the deadline ended the receipt window after 3 of the 15 second window")
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
        update_status(&directory, "working", None, None).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_log_missing"
        );

        let log_path = directory.join(AGY_LOG_FILE);
        fs::write(&log_path, late_reload_startup_log()).unwrap();
        let check = input_receipt_check(Some(&directory));
        assert_eq!(check.reason_code, "agy_startup_not_ready");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["skills_reload_observed"], true);
        assert_eq!(evidence["latest_reload_completed"], false);

        fs::write(&log_path, successful_startup_log()).unwrap();
        assert_eq!(
            input_receipt_check(Some(&directory)).reason_code,
            "agy_no_input_receipt"
        );

        let pending = claim_pending_turn(&directory);
        let mut log = OpenOptions::new().append(true).open(&log_path).unwrap();
        write!(log, "{}", framed_receipt_line("hello", &pending)).unwrap();
        drop(log);
        let check = input_receipt_check(Some(&directory));
        assert_eq!(check.reason_code, "agy_input_receipt_observed");
        let evidence = serde_json::to_value(&check).unwrap()["evidence"].clone();
        assert_eq!(evidence["startup_completed"], true);
        assert_eq!(evidence["latest_reload_completed"], true);
        assert_eq!(evidence["input_receipts"], 1);
        assert_eq!(evidence["last_receipt_truncated"], false);
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
