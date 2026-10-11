use super::delivery::complete_log_lines;
use super::diagnostics::{only_user_input_carries, parse_tool_confirmation};
use super::receipt::{parse_input_receipt, receipt_matches};
use super::{
    FULL_TRANSCRIPT_FILE, PendingAgyTurn, TRANSCRIPT_FILE, correlated_response, read_pending_turn,
};
use crate::native::session::{RecordReader, Store, turn};
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(in crate::native::provider) struct AgyMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl AgyMonitor {
    pub(in crate::native::provider) fn start(directory: &Path, log_path: &Path) -> Result<Self> {
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

    pub(in crate::native::provider) fn stop(mut self) -> Result<()> {
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
pub(super) struct ResultReader<'a> {
    pub(super) full_path: &'a Path,
    pub(super) brain_root: &'a Path,
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

    pub(super) fn contains(&self, paths: &[PathBuf], pending: &PendingAgyTurn) -> Result<bool> {
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

pub(super) fn validated_file_metadata(
    path: &Path,
    brain_root: &Path,
) -> Result<Option<fs::Metadata>> {
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
pub(super) struct PendingTurnLog {
    pub(super) pasted: bool,
    pub(super) failure: Option<String>,
    pub(super) confirmation: Option<String>,
}

pub(super) fn pending_turn_log(log: &[u8], pending: &PendingAgyTurn) -> PendingTurnLog {
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

pub(super) fn pending_turn_failure(log: &[u8], pending: &PendingAgyTurn) -> Option<(String, bool)> {
    let observation = pending_turn_log(log, pending);
    observation.failure.map(|error| (error, observation.pasted))
}

#[derive(Default)]
pub(super) struct MonitorState {
    conversation_id: Option<String>,
    transcript: Option<TranscriptCursor>,
    // The claim whose turn failure has been recorded; its pending-turn file stays
    // until the next turn replaces it.
    failed_claim: Option<String>,
}

impl MonitorState {
    pub(super) fn poll(
        &mut self,
        directory: &Path,
        log_path: &Path,
        brain_root: &Path,
    ) -> Result<()> {
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

pub(super) fn brain_root() -> Result<PathBuf> {
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

pub(super) fn parse_conversation_id(log: &str) -> Option<String> {
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
pub(super) struct PlannerResult {
    pub(super) step: u64,
    pub(super) message: String,
    pub(super) truncated: bool,
}

pub(super) fn parse_planner_result(line: &str) -> Option<PlannerResult> {
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
mod tests;
