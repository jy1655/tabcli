//! Append-only Codex rollout observation. Replace with an addressed, terminal first-party
//! hook when Codex supplies one. Stop candidates never supply completion authority/text.
use super::correlation::begins_with_bridge_prompt;
use super::questions::Questions;
use super::turn::{Binding, ObserverRecord, binding, pending, write_observer};
use super::{PendingCodexTurn, correlated_response};
use crate::native::provider::{ActiveWatcher, CompletionWatcher};
use crate::native::session::primitives::unix_ms;
use crate::native::session::{Reader, Store, turn};
use agent_bridge::FirstPartyCli;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
mod reduce;

// No whole-file ceiling. Each running poll reads at most 256 KiB; a JSONL record
// (excluding LF) or partial tail is retained up to 8 MiB. A longer record continues in
// `reduce` and is skipped only as a validated non-message `response_item`; its in-place
// reduction and final parse are each at most 8 MiB of work within one poll. Shutdown
// snapshots EOF once and drains that finite prefix in bounded chunks.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const CHUNK_BYTES: usize = 256 * 1024;
const LINE_BYTES: usize = 8 * 1024 * 1024;
const ERROR_CHARS: usize = 4096;
const COMPLETE_SOURCE: &str = "codex_transcript_task_complete";
const ABORT_SOURCE: &str = "codex_transcript_turn_aborted";
type Observation<T> = std::result::Result<T, &'static str>;

#[derive(Debug)]
pub(super) struct Watcher;
struct Active {
    stopping: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl CompletionWatcher for Watcher {
    fn start(self: Box<Self>, directory: &Path) -> Result<Box<dyn ActiveWatcher>> {
        let directory = directory.to_owned();
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = stopping.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-codex-transcript".into())
            .spawn(move || {
                let mut observer = Observer::default();
                loop {
                    let drain = flag.load(Ordering::Acquire);
                    observer.poll(&directory, drain);
                    if drain {
                        break;
                    }
                    thread::park_timeout(POLL_INTERVAL);
                }
            })?;
        Ok(Box::new(Active {
            stopping,
            handle: Some(handle),
        }))
    }
}
impl Active {
    fn finish(&mut self) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            handle
                .join()
                .map_err(|_| anyhow!("Codex transcript observer panicked"))?;
        }
        Ok(())
    }
}
impl ActiveWatcher for Active {
    fn stop(mut self: Box<Self>) -> Result<()> {
        self.finish()
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[derive(Default)]
struct Observer {
    cursor: Option<Cursor>,
}
struct Cursor {
    claim: String,
    binding: Option<Binding>,
    pending: Option<PendingCodexTurn>,
    file: Option<File>,
    identity: Option<(u64, u64)>,
    offset: u64,
    extent: u64,
    partial: Vec<u8>,
    overflow: Option<reduce::Reducer>,
    meta_seen: bool,
    accepted: bool,
    questions: Questions,
    suspended: Option<&'static str>,
}
impl Cursor {
    fn new(claim: String) -> Self {
        Self {
            claim,
            binding: None,
            pending: None,
            file: None,
            identity: None,
            offset: 0,
            extent: 0,
            partial: Vec::new(),
            overflow: None,
            meta_seen: false,
            accepted: false,
            questions: Questions::default(),
            suspended: None,
        }
    }
    fn diagnostic(&self, directory: &Path, state: &str, reason: Option<&str>) {
        // State changes only. A diagnostic write failure grants no reporting authority
        // and does not terminate the session loop or cause a repeated write each poll.
        let _ = write_observer(
            &Store::open_unchecked(directory),
            &ObserverRecord {
                schema: 1,
                claim_token: self.claim.clone(),
                state: state.into(),
                reason: reason.map(str::to_owned),
                transcript_path: self.binding.as_ref().map(|b| b.transcript_path.clone()),
                offset: self.offset,
                observed_unix_ms: unix_ms(),
            },
        );
    }
    fn suspend(&mut self, directory: &Path, reason: &'static str) {
        self.suspended = Some(reason);
        self.file = None;
        self.partial.clear();
        self.overflow = None;
        self.diagnostic(
            directory,
            "suspended",
            Some(&format!("observation_unavailable: {reason}")),
        );
    }
    fn open(&mut self) -> Observation<()> {
        let file = open_regular(&self.binding.as_ref().unwrap().transcript_path)?;
        self.identity = Some(file_identity(&file).map_err(|_| "read_error")?);
        self.extent = file.metadata().map_err(|_| "read_error")?.len();
        self.file = Some(file);
        Ok(())
    }
    fn verify(&mut self) -> Observation<u64> {
        let file = open_regular(&self.binding.as_ref().unwrap().transcript_path)?;
        if Some(file_identity(&file).map_err(|_| "read_error")?) != self.identity {
            return Err("replaced");
        }
        let len = file.metadata().map_err(|_| "read_error")?.len();
        if len < self.extent || len < self.offset {
            return Err("truncated");
        }
        // Every successful checkpoint contributes to the high-water mark, even
        // when growth happened after this poll's fixed read/drain snapshot.
        self.extent = len;
        Ok(len)
    }
    fn read(&mut self, drain: bool) -> Observation<Option<Terminal>> {
        let end = self.verify()?;
        let limit = if drain {
            end
        } else {
            end.min(self.offset.saturating_add(CHUNK_BYTES as u64))
        };
        while self.offset < limit {
            let mut bytes = vec![0; (limit - self.offset).min(CHUNK_BYTES as u64) as usize];
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(self.offset))
                .map_err(|_| "read_error")?;
            file.read_exact(&mut bytes).map_err(|_| "read_error")?;
            // Verify after reading, before interpreting or publishing any terminal.
            #[cfg(test)]
            tests::at_read_checkpoint(tests::ReadCheckpoint::BeforePostReadVerify);
            self.verify()?;
            #[cfg(test)]
            tests::at_read_checkpoint(tests::ReadCheckpoint::AfterPostReadVerify);
            for byte in bytes {
                self.offset += 1;
                if byte == b'\n' {
                    if let Some(overflow) = self.overflow.take() {
                        self.skip_oversized(&overflow.finish()?)?;
                        continue;
                    }
                    let line = std::mem::take(&mut self.partial);
                    let text = std::str::from_utf8(&line).map_err(|_| "invalid_utf8")?;
                    let value: Value = serde_json::from_str(text).map_err(|_| "invalid_json")?;
                    if let Some(terminal) = self.record(&value)? {
                        self.verify()?;
                        #[cfg(test)]
                        tests::at_read_checkpoint(tests::ReadCheckpoint::AfterTerminalVerify);
                        return Ok(Some(terminal));
                    }
                } else if let Some(overflow) = &mut self.overflow {
                    overflow.push(byte)?;
                } else if self.partial.len() == LINE_BYTES {
                    let mut overflow =
                        reduce::Reducer::from_prefix(std::mem::take(&mut self.partial))?;
                    overflow.push(byte)?;
                    self.overflow = Some(overflow);
                } else {
                    self.partial.push(byte);
                }
            }
        }
        Ok(None)
    }
    /// Past `LINE_BYTES` only a non-message `response_item` after the verified session
    /// identity is skipped. A cut string reads as `""`, so the discriminators must be
    /// intact and written once; anything else stays oversized.
    fn skip_oversized(&self, reduced: &[u8]) -> Observation<()> {
        let text = std::str::from_utf8(reduced).map_err(|_| "invalid_utf8")?;
        let record: Value = serde_json::from_str(text).map_err(|_| "invalid_json")?;
        let payload = &record["payload"];
        let skip = self.meta_seen
            && payload["type"]
                .as_str()
                .is_some_and(|kind| !kind.is_empty())
            && record["type"]
                .as_str()
                .is_some_and(|kind| unrelated_response_item(kind, payload) == Ok(true))
            && !reduce::repeated_discriminator(text);
        if skip { Ok(()) } else { Err("oversized") }
    }
    fn record(&mut self, record: &Value) -> Observation<Option<Terminal>> {
        let binding = self.binding.as_ref().unwrap();
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .ok_or("invalid_schema")?;
        let payload = &record["payload"];
        if !self.meta_seen {
            if kind != "session_meta" || payload["id"].as_str() != Some(&binding.thread_id) {
                return Err("session_identity");
            }
            self.meta_seen = true;
            return Ok(None);
        }
        if unrelated_response_item(kind, payload)? {
            return Ok(None);
        }
        if kind == "response_item" {
            if payload["role"].as_str().ok_or("invalid_schema")? != "user" {
                return Ok(None);
            }
            // Older records can lack attribution; malformed attribution is not
            // evidence that the delegated input was absent.
            let metadata = &payload["internal_chat_message_metadata_passthrough"];
            if metadata.is_null() {
                return Ok(None);
            }
            let metadata = metadata.as_object().ok_or("invalid_schema")?;
            let Some(turn_id) = metadata.get("turn_id") else {
                return Ok(None);
            };
            if turn_id.as_str().ok_or("invalid_schema")? == binding.turn_id {
                let kinds = metadata
                    .get("content_item_kinds")
                    .and_then(Value::as_array)
                    .ok_or("invalid_schema")?;
                if !kinds.iter().all(Value::is_string) {
                    return Err("invalid_schema");
                }
                let user_text = kinds.iter().any(|kind| kind == "user.text");
                let content = payload["content"].as_array().ok_or("invalid_schema")?;
                for item in content {
                    if !item.is_object() {
                        return Err("invalid_schema");
                    }
                    if item["type"].as_str().ok_or("invalid_schema")? == "input_text" {
                        let text = item["text"].as_str().ok_or("invalid_schema")?;
                        self.accepted |= user_text
                            && begins_with_bridge_prompt(text)
                            && text
                                .trim_end()
                                .ends_with(&self.pending.as_ref().unwrap().marker);
                    }
                }
            }
        }
        if kind != "event_msg" || payload["turn_id"].as_str() != Some(&binding.turn_id) {
            return Ok(None);
        }
        self.questions.notice(payload, binding, self.accepted);
        match payload["type"].as_str() {
            Some("turn_aborted") => {
                let reason = payload["reason"].as_str().ok_or("invalid_schema")?;
                Ok(Some(Terminal {
                    outcome: Err(bounded(reason)),
                    source: ABORT_SOURCE,
                }))
            }
            Some("task_complete" | "turn_complete") => {
                let message = payload
                    .get("last_agent_message")
                    .filter(|v| v.is_null() || v.is_string())
                    .ok_or("invalid_schema")?;
                // Codex omits this optional field on successful rollout records.
                let error = &payload["error"];
                let outcome = if !error.is_null() {
                    Err(bounded(&error.to_string()))
                } else if let Some(message) = message.as_str().filter(|s| !s.trim().is_empty()) {
                    let pending = self.pending.as_ref().unwrap();
                    if message.trim_end().ends_with(&pending.marker) {
                        correlated_response(message, pending)
                            .map(str::to_owned)
                            .map_err(|_| "Codex turn ended without an assistant message".to_owned())
                    } else {
                        Ok(message.to_owned())
                    }
                } else {
                    Err("Codex turn ended without an assistant message".to_owned())
                };
                Ok(Some(Terminal {
                    outcome,
                    source: COMPLETE_SOURCE,
                }))
            }
            _ => Ok(None),
        }
    }
    fn settle(&self, directory: &Path, terminal: Terminal) -> Result<bool> {
        if turn::current_claim_token(&Reader::open_unchecked(directory))?.as_deref()
            != Some(&self.claim)
        {
            return Ok(false);
        }
        let binding = self.binding.as_ref().unwrap();
        let store = Store::open_unchecked(directory);
        let report = turn::Report::for_claim(&store, FirstPartyCli::Codex, Some(&self.claim))
            .with_provenance(Some(terminal.source));
        let outcome = if self.accepted {
            terminal.outcome
        } else {
            Err("Codex did not accept the delegated prompt".into())
        };
        match outcome {
            Ok(message) => report.complete(
                &message,
                Some(binding.thread_id.clone()),
                Some(binding.turn_id.clone()),
            ),
            Err(error) => report.fail(
                &error,
                Some(binding.thread_id.clone()),
                Some(binding.turn_id.clone()),
            ),
        }
        .map(|()| true)
    }
    fn flush_questions(&mut self, directory: &Path, terminal_verified: bool) -> Observation<()> {
        if self.accepted && self.questions.dirty() {
            if !terminal_verified {
                self.verify()?;
            }
            #[cfg(test)]
            tests::at_read_checkpoint(tests::ReadCheckpoint::BeforeQuestionLock);
            self.questions
                .flush(directory, self.binding.as_ref().unwrap());
        }
        Ok(())
    }
}
struct Terminal {
    outcome: std::result::Result<String, String>,
    source: &'static str,
}
impl Observer {
    fn poll(&mut self, directory: &Path, drain: bool) {
        let token = match turn::current_claim_token(&Reader::open_unchecked(directory)) {
            Ok(Some(token)) if turn::valid_turn_claim_token(&token) => token,
            Ok(None) => {
                self.cursor = None;
                return;
            }
            _ => {
                if let Some(cursor) = &mut self.cursor
                    && cursor.suspended.is_none()
                {
                    cursor.suspend(directory, "read_error");
                }
                return;
            }
        };
        if self
            .cursor
            .as_ref()
            .is_none_or(|cursor| cursor.claim != token)
        {
            self.cursor = Some(Cursor::new(token.clone()));
        }
        let cursor = self.cursor.as_mut().unwrap();
        if cursor.suspended.is_some() {
            return;
        }
        let binding = match binding(&Reader::open_unchecked(directory), &token) {
            Ok(binding) => binding,
            Err(_) => {
                cursor.suspend(directory, "binding_conflict");
                return;
            }
        };
        if cursor.binding.is_none() {
            let Some(binding) = binding else {
                self.cursor = None;
                return;
            };
            cursor.binding = Some(binding);
            cursor.pending = match pending(&Reader::open_unchecked(directory)) {
                Ok(Some(pending)) if pending.claim_token == token => Some(pending),
                _ => {
                    cursor.suspend(directory, "binding_conflict");
                    return;
                }
            };
            if let Err(reason) = cursor.open() {
                cursor.suspend(directory, reason);
                return;
            }
            cursor.diagnostic(directory, "armed", None);
        } else if binding != cursor.binding {
            cursor.suspend(directory, "binding_conflict");
            return;
        }
        let read = cursor.read(drain).and_then(|terminal| {
            cursor.flush_questions(directory, terminal.is_some())?;
            Ok(terminal)
        });
        match read {
            Ok(Some(terminal)) => {
                match cursor.settle(directory, terminal) {
                    Err(_) => cursor.suspend(directory, "publication_error"),
                    Ok(reported) => {
                        // Diagnostic only: Report can still reject a stale/duplicate
                        // call under its lock. The committed event owns provenance.
                        if reported {
                            cursor.diagnostic(directory, "settled", None);
                        }
                        self.cursor = None;
                    }
                }
            }
            Ok(None) => {}
            Err(reason) => cursor.suspend(directory, reason),
        }
    }
}
/// A `response_item` other than a message carries no evidence for this observer.
fn unrelated_response_item(kind: &str, payload: &Value) -> Observation<bool> {
    Ok(kind == "response_item" && payload["type"].as_str().ok_or("invalid_schema")? != "message")
}
fn bounded(text: &str) -> String {
    text.chars().take(ERROR_CHARS).collect()
}
fn open_regular(path: &str) -> Observation<File> {
    // Reject non-regular paths before open (notably FIFOs), then verify the handle.
    if !std::fs::metadata(path).map_err(|_| "read_error")?.is_file() {
        return Err("read_error");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "read_error")?;
    if !file.metadata().map_err(|_| "read_error")?.is_file() {
        return Err("read_error");
    }
    Ok(file)
}
fn file_identity(file: &File) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
}
#[cfg(test)]
mod tests;
