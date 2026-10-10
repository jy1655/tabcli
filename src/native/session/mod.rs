//! Views of one session's durable records.
use super::{
    ProviderProcessRecord, RecordedReopenRefusal, SESSION_SCHEMA, STATE_DIR_ENV, SessionEvent,
    SessionManifest, require_valid_session_id, terminal, unix_ms,
};
use crate::native::terminal::ownership::NativeSessionOwner;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use std::{
    thread,
    time::{Duration, Instant},
};
#[cfg(target_os = "macos")]
mod adapter;
pub(in crate::native) mod close;
pub(in crate::native) mod hold;
mod names;
pub(in crate::native) mod requests;
pub(in crate::native) mod turn;
use turn::{JournaledEventRead, JournaledEventState, PendingTurnCompletion};
mod primitives;
mod reads;
#[cfg(test)]
pub(in crate::native) use names::*;
#[cfg(not(test))]
use names::*;
#[cfg(test)]
pub(in crate::native) use primitives::*;
#[cfg(not(test))]
use primitives::*;
#[cfg(test)]
pub(in crate::native) use reads::*;
#[cfg(not(test))]
use reads::*;

/// A read-only view. Opening a view performs no recovery or record mutation.
#[derive(Clone, Debug)]
pub(in crate::native) struct Reader {
    directory: PathBuf,
}

/// A writable view. Lifecycle convergence remains with the command in step one.
pub(in crate::native) struct Store {
    reader: Reader,
}

impl std::ops::Deref for Store {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        &self.reader
    }
}

/// An adapter owns the name and schema; the store owns its I/O primitives.
pub(in crate::native) struct RecordReader {
    path: PathBuf,
}
pub(in crate::native) struct RecordStore {
    reader: RecordReader,
}
impl std::ops::Deref for RecordStore {
    type Target = RecordReader;
    fn deref(&self) -> &RecordReader {
        &self.reader
    }
}
impl RecordReader {
    pub(in crate::native) fn raw_text(&self) -> std::io::Result<String> {
        fs::read_to_string(&self.path)
    }

    /// A record whose address has already been resolved by its owner. No I/O or validation.
    pub(in crate::native) fn at(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    pub(in crate::native) fn child(&self, name: &str) -> RecordReader {
        RecordReader {
            path: self.path.join(name),
        }
    }
    pub(in crate::native) fn timeline(&self, limit: u64) -> Result<Option<Vec<u8>>> {
        read_timeline_record(&self.path, limit)
    }
    pub(in crate::native) fn event_within_budget(
        &self,
        limit: u64,
    ) -> Result<Option<String>, Result<u64>> {
        read_event_within_budget(&self.path, limit)
    }
    pub(in crate::native) fn path(&self) -> &Path {
        &self.path
    }
    pub(in crate::native) fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        read_json(&self.path)
    }
    pub(in crate::native) fn text(&self) -> Result<Option<String>> {
        read_regular_text_if_present(&self.path)
    }
    pub(in crate::native) fn bytes(&self) -> Result<Option<Vec<u8>>> {
        read_regular_bytes_if_present(&self.path)
    }
}
impl RecordStore {
    /// Preserve addressed records held by lifecycle guards without reconstructing their names.
    pub(in crate::native) fn at(path: &Path) -> Self {
        Self {
            reader: RecordReader::at(path),
        }
    }

    pub(in crate::native) fn child(&self, name: &str) -> RecordStore {
        RecordStore {
            reader: self.reader.child(name),
        }
    }

    pub(in crate::native) fn write_json<T: Serialize>(&self, value: &T) -> Result<()> {
        write_json_atomic(&self.path, value)
    }
    pub(in crate::native) fn write_private(&self, bytes: &[u8]) -> Result<()> {
        write_private(&self.path, bytes)
    }
    pub(in crate::native) fn remove(&self) -> Result<()> {
        remove_file_if_present(&self.path)
    }
}

impl Reader {
    pub(in crate::native) fn open_unchecked(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_owned(),
        }
    }
    pub(in crate::native) fn directory(&self) -> &Path {
        &self.directory
    }
    pub(in crate::native) fn private(&self, name: &str) -> RecordReader {
        RecordReader {
            path: self.directory.join(name),
        }
    }
    pub(in crate::native) fn manifest(&self) -> Result<SessionManifest> {
        read_manifest(&self.directory)
    }
    pub(in crate::native) fn closed_if_present(&self) -> Result<Option<SessionStatus>> {
        read_status_if_present(&self.directory.join(CLOSED_STATUS_FILE))
    }
    pub(in crate::native) fn event(&self, name: &str) -> RecordReader {
        self.private(EVENTS_DIRECTORY).child(name)
    }
    pub(in crate::native) fn event_strict(
        &self,
        name: &str,
    ) -> std::result::Result<SessionEvent, String> {
        read_event_strictly(&self.directory, name)
    }
    pub(in crate::native) fn status_if_present(&self) -> Result<Option<SessionStatus>> {
        read_status_if_present(&self.directory.join(STATUS_FILE))
    }
    pub(in crate::native) fn regular_closed_if_present(&self) -> Result<Option<SessionStatus>> {
        read_regular_status_if_present(&self.directory.join(CLOSED_STATUS_FILE))
    }
    pub(in crate::native) fn initial_prompt(&self) -> std::io::Result<String> {
        fs::read_to_string(self.directory.join(INITIAL_PROMPT_FILE))
    }
    pub(in crate::native) fn regular_status_if_present(&self) -> Result<Option<SessionStatus>> {
        read_regular_status_if_present(&self.directory.join(STATUS_FILE))
    }
    pub(in crate::native) fn events(&self) -> Result<Vec<PathBuf>> {
        event_paths(&self.directory)
    }
    pub(in crate::native) fn status(&self) -> Result<SessionStatus> {
        self.private(STATUS_FILE).json()
    }
    // Read by the macOS and Windows ownership paths; Linux has no managed surface yet.
    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn owner(&self) -> Result<NativeSessionOwner> {
        self.private(SESSION_OWNER_FILE).json()
    }
    pub(in crate::native) fn terminal(&self) -> Result<terminal::TerminalSession> {
        self.private(TERMINAL_HANDLE_FILE).json()
    }
    pub(in crate::native) fn terminal_closed(&self) -> Result<terminal::TerminalSession> {
        self.private(TERMINAL_TOMBSTONE_FILE).json()
    }
    pub(in crate::native) fn provider_process(&self) -> Result<ProviderProcessRecord> {
        self.private(PROVIDER_PROCESS_FILE).json()
    }
    pub(in crate::native) fn lock_shared(&self) -> Result<Option<File>> {
        let directory = self.directory();
        // Open an existing lifecycle lock without creating it or changing permissions.
        let lock_path = directory.join(TURN_CLAIM_LOCK_FILE);
        let lock = match File::open(&lock_path) {
            Ok(file) => {
                match file.try_lock_shared() {
                    Ok(()) => (),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        return Err(SnapshotBusy.into());
                    }
                    Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                }
                Some(file)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("failed to observe native lifecycle lock"),
        };
        Ok(lock)
    }
    pub(in crate::native) fn lock_shared_with_retry(
        &self,
        retry_window: Duration,
    ) -> Result<Option<File>> {
        let directory = self.directory();
        // Keep the lifecycle reader lock while collecting and checking retained bytes.
        let lock = match File::open(directory.join(TURN_CLAIM_LOCK_FILE)) {
            Ok(file) => {
                let deadline = Instant::now() + retry_window;
                loop {
                    match file.try_lock_shared() {
                        Ok(()) => break,
                        Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(25))
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {
                            return Err(SnapshotBusy.into());
                        }
                        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                    }
                }
                Some(file)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("failed to observe native lifecycle lock"),
        };
        Ok(lock)
    }
}

impl Store {
    pub(in crate::native) fn create_claim_file(&self) -> Result<File> {
        let path = self.directory.join(TURN_CLAIM_FILE);
        fault_point("creating the turn claim")?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| "another tell request already owns this session turn")?;
        set_private_file_permissions(&file)?;
        Ok(file)
    }
    pub(in crate::native) fn write_claim_token(&self, file: &mut File, token: &str) -> Result<()> {
        writeln!(file, "{token}")?;
        file.flush()?;
        sync_file(file, &self.directory.join(TURN_CLAIM_FILE))?;
        sync_parent_directory(&self.directory.join(TURN_CLAIM_FILE))?;
        Ok(())
    }

    pub(in crate::native) fn open_unchecked(directory: impl AsRef<Path>) -> Self {
        Self {
            reader: Reader::open_unchecked(directory),
        }
    }
    pub(in crate::native) fn private(&self, name: &str) -> RecordStore {
        RecordStore {
            reader: self.reader.private(name),
        }
    }
    pub(in crate::native) fn lock_status(&self) -> Result<StatusLock> {
        lock_status(&self.directory)
    }
    pub(in crate::native) fn lock(&self) -> Result<TurnClaimLock> {
        lock_turn_claim(&self.directory.join(TURN_CLAIM_FILE))
    }
    pub(in crate::native) fn try_lock(&self) -> Result<Option<TurnClaimLock>> {
        try_lock(&self.directory)
    }
    #[cfg(test)]
    pub(in crate::native) fn write_status(&self, value: &SessionStatus) -> Result<()> {
        self.private(STATUS_FILE).write_json(value)
    }
    pub(in crate::native) fn write_owner(&self, value: &NativeSessionOwner) -> Result<()> {
        self.private(SESSION_OWNER_FILE).write_json(value)
    }
    pub(in crate::native) fn write_completion(&self, value: &PendingTurnCompletion) -> Result<()> {
        self.private(TURN_COMPLETION_FILE).write_json(value)
    }
    #[cfg(test)]
    pub(in crate::native) fn write_closed(&self, value: &SessionStatus) -> Result<()> {
        self.private(CLOSED_STATUS_FILE).write_json(value)
    }
    pub(in crate::native) fn write_terminal(
        &self,
        value: &terminal::TerminalSession,
    ) -> Result<()> {
        self.private(TERMINAL_HANDLE_FILE).write_json(value)
    }
    pub(in crate::native) fn write_terminal_closed(&self, value: &serde_json::Value) -> Result<()> {
        self.private(TERMINAL_TOMBSTONE_FILE).write_json(value)
    }
    pub(in crate::native) fn write_reopen_refusal(
        &self,
        value: &RecordedReopenRefusal,
    ) -> Result<()> {
        self.private(REOPEN_REFUSAL_FILE).write_json(value)
    }
    pub(in crate::native) fn write_provider_process(
        &self,
        value: &ProviderProcessRecord,
    ) -> Result<()> {
        self.private(PROVIDER_PROCESS_FILE).write_json(value)
    }
    pub(in crate::native) fn write_manifest(&self, value: &SessionManifest) -> Result<()> {
        self.private(MANIFEST_FILE).write_json(value)
    }
}

#[derive(Debug)]
pub(in crate::native) struct SnapshotBusy;
impl std::fmt::Display for SnapshotBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session records are changing; retry the read-only query")
    }
}
impl std::error::Error for SnapshotBusy {}

#[derive(Clone, Copy)]
pub(in crate::native) enum CoreRecord {
    Hold,
    TurnClaim,
    Completion,
    Owner,
    Closed,
    Terminal,
    TerminalClosing,
    TerminalClosed,
    #[cfg(target_os = "macos")]
    TerminalCloseIntent,
    LegacyResumePending,
    LegacyResumeRunning,
    ReopenMarker,
    ReopenRefusal,
    ProviderProcess,
    Status,
    Manifest,
    InitialPrompt,
    Events,
    Requests,
    Launch,
}
impl CoreRecord {
    pub(in crate::native) fn name(self) -> &'static str {
        match self {
            Self::Hold => HOLD_FILE,
            Self::TurnClaim => TURN_CLAIM_FILE,
            Self::Completion => TURN_COMPLETION_FILE,
            Self::Owner => SESSION_OWNER_FILE,
            Self::Closed => CLOSED_STATUS_FILE,
            Self::Terminal => TERMINAL_HANDLE_FILE,
            Self::TerminalClosing => TERMINAL_CLOSING_FILE,
            Self::TerminalClosed => TERMINAL_TOMBSTONE_FILE,
            #[cfg(target_os = "macos")]
            Self::TerminalCloseIntent => TERMINAL_CLOSE_INTENT_FILE,
            Self::LegacyResumePending => LEGACY_RESUME_PENDING_FILE,
            Self::LegacyResumeRunning => LEGACY_RESUME_RUNNING_FILE,
            Self::ReopenMarker => REOPEN_MARKER_FILE,
            Self::ReopenRefusal => REOPEN_REFUSAL_FILE,
            Self::ProviderProcess => PROVIDER_PROCESS_FILE,
            Self::Status => STATUS_FILE,
            Self::Manifest => MANIFEST_FILE,
            Self::InitialPrompt => INITIAL_PROMPT_FILE,
            Self::Events => EVENTS_DIRECTORY,
            Self::Requests => REQUESTS_DIRECTORY,
            Self::Launch => LAUNCH_FILE,
        }
    }
}

impl Reader {
    pub(in crate::native) fn record(&self, record: CoreRecord) -> RecordReader {
        self.private(record.name())
    }
    pub(in crate::native) fn state_root() -> Result<PathBuf> {
        state_root()
    }
    pub(in crate::native) fn session_directory(id: &str) -> Result<PathBuf> {
        session_directory(id)
    }
    pub(in crate::native) fn session_directory_in(root: &Path, id: &str) -> Result<PathBuf> {
        session_directory_in(root, id)
    }
    pub(in crate::native) fn validate_hook_directory(&self) -> Result<()> {
        validate_hook_directory(&self.directory)
    }
    pub(in crate::native) fn events_directory_state(&self) -> Result<EventsDirectory> {
        events_directory_state(&self.directory)
    }
    pub(in crate::native) fn require_events_directory(&self) -> Result<()> {
        require_events_directory(&self.directory)
    }
    pub(in crate::native) fn journaled_event_state_within(
        &self,
        pending: &PendingTurnCompletion,
        limit: u64,
    ) -> Result<JournaledEventRead> {
        journaled_event_state_within(&self.directory, pending, limit)
    }
    pub(in crate::native) fn valid_event_file_name(name: &str) -> bool {
        valid_event_file_name(name)
    }
    pub(in crate::native) fn home_directories() -> Vec<PathBuf> {
        home_directories()
    }
}
impl Store {
    pub(in crate::native) fn record(&self, record: CoreRecord) -> RecordStore {
        self.private(record.name())
    }
    pub(in crate::native) fn create_directory(
        root: &Path,
        durable: &[PathBuf],
    ) -> Result<(PathBuf, String, Option<String>)> {
        create_directory(root, durable)
    }
    pub(in crate::native) fn create_state_root(
        root: &Path,
        durable: &[PathBuf],
    ) -> Result<Option<String>> {
        create_state_root(root, durable)
    }
    pub(in crate::native) fn new_event_file_name() -> Result<String> {
        new_event_file_name()
    }
}
impl RecordReader {
    pub(in crate::native) fn optional_json<T: for<'de> Deserialize<'de>>(
        &self,
    ) -> Result<Option<T>> {
        read_regular_text_if_present(&self.path)?
            .map(|text| {
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid JSON in {}", self.path.display()))
            })
            .transpose()
    }
    pub(in crate::native) fn is_regular_file(&self) -> Result<bool> {
        is_regular_file(&self.path)
    }
}
impl RecordStore {
    pub(in crate::native) fn rename_to(&self, to: &RecordStore) -> Result<()> {
        rename_session_file(&self.path, &to.path)
    }
    pub(in crate::native) fn remove_raw(&self) -> std::io::Result<()> {
        fs::remove_file(&self.path)
    }
    pub(in crate::native) fn remove_directory_all(&self) -> std::io::Result<()> {
        fs::remove_dir_all(&self.path)
    }
    pub(in crate::native) fn set_file_private(file: &File) -> Result<()> {
        set_private_file_permissions(file)
    }
    pub(in crate::native) fn set_directory_private(&self) -> Result<()> {
        set_private_directory_permissions(&self.path)
    }
    #[cfg(windows)]
    pub(in crate::native) fn persist(
        &self,
        temporary: tempfile::NamedTempFile,
    ) -> std::io::Result<File> {
        persist_record(temporary, &self.path)
    }
}

#[cfg(not(test))]
pub(in crate::native) use primitives::{EventsDirectory, StatusLock, TurnClaimLock};

impl Store {
    pub(in crate::native) fn claim_terminal_handle(&self) -> Result<std::io::Result<()>> {
        let terminal_path = self.directory.join(TERMINAL_HANDLE_FILE);
        let closing_path = self.directory.join(TERMINAL_CLOSING_FILE);
        fault_point("claiming the terminal handle for close")?;
        let result = fs::rename(&terminal_path, &closing_path);
        if result.is_ok() {
            fault_point("syncing the claimed terminal handle's directory")?;
            sync_parent_directory(&closing_path)?;
        }
        Ok(result)
    }
    pub(in crate::native) fn unpublished_event(&self, name: &str) -> RecordStore {
        self.private(EVENTS_DIRECTORY)
            .child(&format!("{UNPUBLISHED_EVENT_PREFIX}{name}"))
    }
    pub(in crate::native) fn sync_set_aside_event_directory(&self) -> Result<()> {
        fault_point("syncing a set-aside completion event's directory")?;
        let events = self.directory.join(EVENTS_DIRECTORY);
        sync_directory(&events)
            .with_context(|| format!("failed to sync state directory {}", events.display()))
    }
    pub(in crate::native) fn sync_committed_event_directory(&self) -> Result<()> {
        fault_point("syncing a committed completion event's directory")?;
        let events = self.directory.join(EVENTS_DIRECTORY);
        sync_directory(&events)
            .with_context(|| format!("failed to sync state directory {}", events.display()))
    }
}

impl RecordStore {
    /// Exclusive, compact evidence with no flush or fsync: used by Claude's timed hooks.
    pub(in crate::native) fn write_json_if_absent<T: Serialize>(&self, value: &T) -> Result<()> {
        let path = &self.path;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        set_private_file_permissions(&file)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        Ok(())
    }
    /// Appends the bytes `line` produces. The producer runs after the handle is open, so a
    /// timestamp it samples belongs to the write, not to the attempt to open the record.
    pub(in crate::native) fn append(&self, line: impl FnOnce() -> Vec<u8>) -> Result<()> {
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(&line())?;
        Ok(())
    }
}

impl RecordReader {
    pub(in crate::native) fn open(&self) -> std::io::Result<File> {
        OpenOptions::new().read(true).open(&self.path)
    }
    // Read by the WezTerm and Windows paths; Linux has no managed surface that needs it.
    #[cfg(any(target_os = "macos", windows))]
    pub(in crate::native) fn raw_bytes(&self) -> std::io::Result<Vec<u8>> {
        fs::read(&self.path)
    }
}
impl RecordStore {
    #[cfg(windows)]
    pub(in crate::native) fn create_new(&self) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
    }
}

#[cfg(test)]
pub(super) mod tests;

mod owner;
mod state;
pub(in crate::native) use owner::{OwnerObservation, observe_owner, observe_owner_record};
pub(in crate::native) use state::{
    SessionState, SessionStatus, update_status, update_status_with_residual,
};
pub(in crate::native) mod launch;

impl Store {
    /// Publish an accepted completion before repairing its session's dead owner.
    pub(in crate::native) fn converge(&self) -> Result<()> {
        // Repair owns recovery and preserves its damage while settling a dead owner.
        // An earlier recovery here both duplicated work and skipped that settlement
        // when a completion record could not be published.
        close::repair_dead_owner(self)?;
        Ok(())
    }
}
