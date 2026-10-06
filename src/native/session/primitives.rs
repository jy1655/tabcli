//! Record I/O and durability; lifecycle decisions remain in native.
use super::*;

pub(in crate::native) fn is_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

pub(in crate::native) fn read_regular_status_if_present(
    path: &Path,
) -> Result<Option<SessionStatus>> {
    let Some(text) = read_regular_text_if_present(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

pub(in crate::native) fn remove_file_if_present(path: &Path) -> Result<()> {
    fault_point("removing a record file")?;
    match fs::remove_file(path) {
        Ok(()) => {
            fault_point("syncing a removed record's directory")?;
            sync_parent_directory(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub(in crate::native) fn rename_session_file(from: &Path, to: &Path) -> Result<()> {
    fault_point("renaming a record file")?;
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
    fault_point("syncing a renamed record's directory")?;
    sync_parent_directory(to)?;
    if from.parent() != to.parent() {
        sync_parent_directory(from)?;
    }
    Ok(())
}

pub(in crate::native) fn read_regular_text_if_present(path: &Path) -> Result<Option<String>> {
    Ok(read_regular_bytes_if_present(path)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// The raw bytes of a regular session file, so callers that must not alter a record can
/// decode it strictly instead of through the lossy snapshot reader.
pub(in crate::native) fn read_regular_bytes_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    fs::read(path)
        .map(Some)
        .with_context(|| format!("failed to read {}", path.display()))
}

/// The receipt stored at `STATE_ROOT_DURABLE_FILE`. Only its existence as a regular file
/// carries meaning; the fields describe the walk that wrote it.
#[derive(Serialize)]
pub(in crate::native) struct StateRootDurabilityReceipt {
    schema: u32,
    synced_unix_ms: u128,
}

/// Creates the state root and every missing ancestor, then makes the root's ancestry
/// durable unless the durability receipt already proves it is. A directory entry is a
/// record like the files inside it: the session directory is only durable once the root's
/// entry is, and the root's entry is only durable once every ancestor's entry is.
///
/// The walk does not depend on who created the directories. A creator that made the root
/// or an ancestor and stopped before the parent-directory syncs leaves an existing root
/// whose ancestry is not durable, and a creator that probed while another was still
/// creating sees only part of what the other made. So every creation that finds no
/// receipt syncs the entry of the root and of each ancestor above it, nearest first, up to
/// and including the entry that sits directly in the filesystem root or in one of
/// `durable_directories`, whichever comes first, bounded by
/// `STATE_ROOT_ANCESTRY_SYNC_LIMIT` entries. Concurrent creators may both walk; the syncs
/// are idempotent. The receipt is written through [`write_json_atomic`], which also syncs
/// the root, only after the walk succeeded.
///
/// Returns the walk's failure, if any, for the session's launch status: a missing receipt
/// never blocks creation, and the receipt stays absent so the next creation walks again.
pub(in crate::native) fn create_state_root(
    root: &Path,
    durable_directories: &[PathBuf],
) -> Result<Option<String>> {
    let mut created = Vec::new();
    let mut probe = root;
    loop {
        match fs::symlink_metadata(probe) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                created.push(probe.to_path_buf());
                match probe.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
                    _ => break,
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect state directory {}", probe.display())
                });
            }
        }
    }
    for directory in created.iter().rev() {
        // A concurrent creator may win the race; the ancestry walk below covers its
        // entries and this creator's alike.
        if let Err(error) = fs::create_dir(directory)
            && !directory.is_dir()
        {
            return Err(error).with_context(|| {
                format!("failed to create state directory {}", directory.display())
            });
        }
    }
    set_private_directory_permissions(root)?;
    if state_root_durability_receipt_present(root) {
        return Ok(None);
    }
    fault_point("syncing the state root's ancestry")?;
    if let Err(error) = sync_state_root_ancestry(root, durable_directories) {
        return Ok(Some(format!(
            "state root ancestry was not made durable: {error:#}"
        )));
    }
    let receipt = StateRootDurabilityReceipt {
        schema: 1,
        synced_unix_ms: unix_ms(),
    };
    write_json_atomic(&root.join(STATE_ROOT_DURABLE_FILE), &receipt)?;
    Ok(None)
}

/// Whether the state root carries its durability receipt. Only a regular file counts; a
/// missing, unreadable, or non-regular entry means the ancestry walk runs again, which is
/// harmless when the ancestry was in fact durable.
pub(in crate::native) fn state_root_durability_receipt_present(root: &Path) -> bool {
    fs::symlink_metadata(root.join(STATE_ROOT_DURABLE_FILE))
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

/// Syncs the directory that holds the entry of `root`, then the one that holds its
/// parent's entry, and so on. The walk stops after the sync that makes durable an entry
/// sitting directly in the filesystem root or in one of `durable_directories`, whose own
/// entries are not the bridge's to establish, or after `STATE_ROOT_ANCESTRY_SYNC_LIMIT`
/// entries.
pub(in crate::native) fn sync_state_root_ancestry(
    root: &Path,
    durable_directories: &[PathBuf],
) -> Result<()> {
    let mut entry = root;
    for _ in 0..STATE_ROOT_ANCESTRY_SYNC_LIMIT {
        let Some(parent) = entry.parent() else {
            break;
        };
        let holder = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        sync_directory(holder)
            .with_context(|| format!("failed to sync state directory {}", holder.display()))?;
        let holder_is_filesystem_root = parent.as_os_str().is_empty() || parent.parent().is_none();
        if holder_is_filesystem_root
            || durable_directories
                .iter()
                .any(|durable| same_directory(parent, durable))
        {
            break;
        }
        entry = parent;
    }
    Ok(())
}

/// Whether two paths name the same directory, by spelling or after canonicalisation.
pub(in crate::native) fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

/// The directories whose own entries the state-root ancestry walk takes as durable: the
/// user's home directory under either of the variables `default_state_root` reads.
pub(in crate::native) fn home_directories() -> Vec<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub(in crate::native) fn state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(root));
    }
    default_state_root(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

pub(in crate::native) fn default_state_root(
    home: Option<&std::ffi::OsStr>,
    user_profile: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let home = home
        .or(user_profile)
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")?;
    Ok(home.join(".agent-bridge").join("native-sessions"))
}

pub(in crate::native) fn session_directory(id: &str) -> Result<PathBuf> {
    session_directory_in(&state_root()?, id)
}

pub(in crate::native) fn session_directory_in(root: &Path, id: &str) -> Result<PathBuf> {
    require_valid_session_id(id)?;
    let directory = root.join(id);
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("no such Agent Bridge session: {id}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("refusing non-directory Agent Bridge session: {id}");
    }
    Ok(directory)
}

pub(in crate::native) fn validate_hook_directory(directory: &Path) -> Result<()> {
    let root = state_root()?
        .canonicalize()
        .context("native state root is missing")?;
    let canonical = directory
        .canonicalize()
        .context("native hook session directory is missing")?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "refusing native hook directory outside state root: {}",
            directory.display()
        );
    }
    let id = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .context("native hook session id is not UTF-8")?;
    require_valid_session_id(id)
}

pub(in crate::native) fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    // The manifest decides a session's scope (workspace, provider) for every read-only
    // query, so it is read like every other session record: a link at `manifest.json`
    // would let content outside the state root steer a search or an attach, and is
    // refused rather than followed.
    let path = directory.join(crate::native::session::MANIFEST_FILE);
    let bytes = read_regular_bytes_if_present(&path)?
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: SessionManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    if manifest.schema != SESSION_SCHEMA {
        bail!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        );
    }
    let expected_id = directory.file_name().and_then(|name| name.to_str());
    if expected_id != Some(manifest.id.as_str()) {
        bail!("session manifest id does not match its directory");
    }
    Ok(manifest)
}

pub(in crate::native) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

// Durable record writes. Every helper below syncs the file it changed and then the
// directory that holds its entry, so a crash after the helper returns cannot lose the
// record on a POSIX file system that honours fsync. See README "권한과 세션 경계" for the
// classification of which records go through these helpers and the platform limits.
//
// Under `cfg(test)` three thread-local hooks observe these helpers: `fault_point` refuses
// the next filesystem mutation once an injected budget is spent, which models a process
// that died between two mutations, `record_sync` logs every sync call in order, and
// `sync_directory` refuses the directories named by `with_sync_failure`, which models a
// sync the operating system rejects. All are inert outside tests.

#[cfg(test)]
thread_local! {
    static FAULT_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static SYNC_LOG: std::cell::RefCell<Option<Vec<SyncRecord>>> =
        const { std::cell::RefCell::new(None) };
    /// Directories whose sync fails with an injected error while `with_sync_failure` runs.
    static SYNC_FAILURES: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Every journaled event the publication predicate opened, in order, so a test can
    /// prove when a search reads a journaled event and when it does not read it at all.
    static PUBLICATION_READ_LOG: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum SyncRecord {
    File(PathBuf),
    Directory(PathBuf),
}

/// Refuses the step that follows it once the injected fault budget reaches zero. A budget of
/// `k` lets exactly `k` boundaries pass and then fails every later one, like a process that
/// stopped there. Boundaries sit before each record mutation (temporary-file creation,
/// permission and content writes, rename, removal) and between a rename or removal and the
/// sync that makes it durable; a fault after a temporary file exists leaves it behind.
pub(in crate::native) fn fault_point(label: &str) -> Result<()> {
    #[cfg(test)]
    {
        FAULT_BUDGET.with(|budget| match budget.get() {
            None => Ok(()),
            Some(0) => bail!("injected fault before {label}"),
            Some(remaining) => {
                budget.set(Some(remaining - 1));
                Ok(())
            }
        })
    }
    #[cfg(not(test))]
    {
        let _ = label;
        Ok(())
    }
}

#[cfg(test)]
pub(in crate::native) fn injected_fault(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("injected fault before")
}

#[cfg(test)]
pub(in crate::native) fn with_fault_budget<T>(budget: usize, run: impl FnOnce() -> T) -> T {
    FAULT_BUDGET.with(|cell| cell.set(Some(budget)));
    let outcome = run();
    FAULT_BUDGET.with(|cell| cell.set(None));
    outcome
}

#[cfg(test)]
pub(in crate::native) fn with_sync_log<T>(run: impl FnOnce() -> T) -> (T, Vec<SyncRecord>) {
    SYNC_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
    let outcome = run();
    let records = SYNC_LOG.with(|log| log.borrow_mut().take().unwrap_or_default());
    (outcome, records)
}

/// Runs `run` while every sync of `directory` on this thread fails with an injected
/// error. The sync is still logged first, so a test sees that it was attempted.
#[cfg(test)]
pub(in crate::native) fn with_sync_failure<T>(directory: &Path, run: impl FnOnce() -> T) -> T {
    SYNC_FAILURES.with(|failures| failures.borrow_mut().push(directory.to_path_buf()));
    let outcome = run();
    SYNC_FAILURES.with(|failures| failures.borrow_mut().clear());
    outcome
}

#[cfg(test)]
pub(in crate::native) fn injected_sync_failure(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("injected sync failure for")
}

/// Runs `run` and returns, in order, the path of every journaled event the publication
/// predicate opened on this thread while it ran.
#[cfg(test)]
pub(in crate::native) fn with_publication_read_log<T>(
    run: impl FnOnce() -> T,
) -> (T, Vec<PathBuf>) {
    PUBLICATION_READ_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
    let outcome = run();
    let paths = PUBLICATION_READ_LOG.with(|log| log.borrow_mut().take().unwrap_or_default());
    (outcome, paths)
}

pub(in crate::native) fn record_publication_read(path: &Path) {
    #[cfg(test)]
    PUBLICATION_READ_LOG.with(|log| {
        if let Some(log) = log.borrow_mut().as_mut() {
            log.push(path.to_path_buf());
        }
    });
    #[cfg(not(test))]
    let _ = path;
}

#[derive(Clone, Copy)]
pub(in crate::native) enum SyncKind {
    File,
    Directory,
}

pub(in crate::native) fn record_sync(kind: SyncKind, path: &Path) {
    #[cfg(test)]
    SYNC_LOG.with(|log| {
        if let Some(log) = log.borrow_mut().as_mut() {
            log.push(match kind {
                SyncKind::File => SyncRecord::File(path.to_path_buf()),
                SyncKind::Directory => SyncRecord::Directory(path.to_path_buf()),
            });
        }
    });
    #[cfg(not(test))]
    {
        let _ = (kind, path);
    }
}

pub(in crate::native) fn sync_file(file: &File, path: &Path) -> Result<()> {
    record_sync(SyncKind::File, path);
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

pub(in crate::native) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    fault_point("creating a temporary record file")?;
    let temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    let mut temporary = fault_point_keeping_temporary(
        "writing a temporary record's permissions and content",
        temporary,
    )?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.flush()?;
    sync_file(temporary.as_file(), temporary.path())?;
    let temporary = fault_point_keeping_temporary(
        "renaming a temporary record over its final path",
        temporary,
    )?;
    let persisted = persist_record(temporary, path)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    fault_point("syncing a renamed record")?;
    sync_file(&persisted, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

// How long a record replacement waits for another handle to the record to close.
#[cfg(windows)]
const RECORD_REPLACE_WAIT: Duration = Duration::from_secs(1);

/// Renames a temporary record over its final path. Windows refuses to replace a file
/// while any other handle to it is open, whatever that handle shares, and a caller that
/// polls a record holds one for a moment on every read (2026-10-01: a launcher's read of
/// `launch.json` failed the wrapper's replacement of it with access denied). The rename
/// is therefore repeated for a bounded time on those two errors; a record that stays
/// open, or cannot be replaced for another reason, still fails.
pub(in crate::native) fn persist_record(
    temporary: tempfile::NamedTempFile,
    path: &Path,
) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
        let deadline = Instant::now() + RECORD_REPLACE_WAIT;
        let mut temporary = temporary;
        loop {
            match temporary.persist(path) {
                Ok(file) => return Ok(file),
                Err(error)
                    if Instant::now() < deadline
                        && error.error.raw_os_error().is_some_and(|code| {
                            [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION].contains(&(code as u32))
                        }) =>
                {
                    temporary = error.file;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.error),
            }
        }
    }
    #[cfg(not(windows))]
    {
        temporary.persist(path).map_err(|error| error.error)
    }
}

/// A fault at a boundary after the temporary file exists leaves that file behind, exactly
/// as an abrupt stop would; ordinary errors still remove it when the handle drops.
pub(in crate::native) fn fault_point_keeping_temporary(
    label: &str,
    temporary: tempfile::NamedTempFile,
) -> Result<tempfile::NamedTempFile> {
    match fault_point(label) {
        Ok(()) => Ok(temporary),
        Err(error) => {
            #[cfg(test)]
            {
                let _ = temporary.keep();
            }
            Err(error)
        }
    }
}

pub(in crate::native) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    fault_point("creating a private record file")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // The file now exists at its final path; a fault here leaves it empty, as a stop would.
    fault_point("writing a private record's permissions and content")?;
    set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    fault_point("syncing a private record")?;
    sync_file(&file, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

pub(in crate::native) fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    sync_directory(parent)
        .with_context(|| format!("failed to sync state directory {}", parent.display()))
}

pub(in crate::native) fn sync_directory(directory: &Path) -> Result<()> {
    record_sync(SyncKind::Directory, directory);
    #[cfg(test)]
    if SYNC_FAILURES.with(|failures| failures.borrow().iter().any(|failed| failed == directory)) {
        bail!("injected sync failure for {}", directory.display());
    }
    sync_directory_entries(directory)
}

#[cfg(unix)]
pub(in crate::native) fn sync_directory_entries(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

// Windows flushes the directory's metadata through a handle opened with backup semantics.
// NTFS journals directory entries, so this is a best-effort flush of the volume's cached
// metadata rather than the POSIX guarantee that the entry itself reached stable storage.
#[cfg(windows)]
pub(in crate::native) fn sync_directory_entries(directory: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?
        .sync_all()?;
    Ok(())
}

// No supported transport exists on other targets; the directory entry is left to the
// operating system's own write-back and the records are not claimed durable there.
#[cfg(not(any(unix, windows)))]
pub(in crate::native) fn sync_directory_entries(_directory: &Path) -> Result<()> {
    Ok(())
}

pub(in crate::native) struct StatusLock {
    _file: File,
}

pub(in crate::native) fn lock_status(directory: &Path) -> Result<StatusLock> {
    let path = directory.join(STATUS_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    set_private_file_permissions(&file)?;
    file.lock()
        .with_context(|| "failed to lock native session status")?;
    Ok(StatusLock { _file: file })
}

pub(in crate::native) fn read_status_if_present(path: &Path) -> Result<Option<SessionStatus>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub(in crate::native) struct TurnClaimLock {
    _file: File,
}

pub(in crate::native) fn lock_turn_claim(path: &Path) -> Result<TurnClaimLock> {
    let lock_path = path.with_file_name(TURN_CLAIM_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    set_private_file_permissions(&file)?;
    file.lock()
        .with_context(|| "failed to lock native turn claim lifecycle")?;
    Ok(TurnClaimLock { _file: file })
}

/// What stands at a session's `events` path when it is safe to say anything about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum EventsDirectory {
    /// A real directory inside the session directory.
    Present,
    /// Nothing at all: the session holds no event record.
    Missing,
}

/// Inspects a session's `events` path with `symlink_metadata`, so a symlink or Windows
/// junction is rejected rather than followed, as is a non-directory file or an unreadable
/// entry. A missing path is reported, not rejected: readers and lifecycle steps decide
/// what an absent directory means for them.
pub(in crate::native) fn events_directory_state(directory: &Path) -> Result<EventsDirectory> {
    let events = directory.join(crate::native::session::EVENTS_DIRECTORY);
    match fs::symlink_metadata(&events) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("events directory is a symlink")
        }
        Ok(metadata) if !metadata.is_dir() => bail!("events is not a directory"),
        Ok(_) => Ok(EventsDirectory::Present),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(EventsDirectory::Missing),
        Err(error) => bail!("events directory is unreadable: {error}"),
    }
}

/// A session's `events` must be a real directory inside the session directory before any
/// record under it is opened: the shared event listing turns a missing or non-directory
/// `events` path into an empty list, a search must not present that damage as "no
/// results", and a link planted there would carry a publication read outside the state
/// root. Built on [`events_directory_state`], so a link is rejected rather than followed.
pub(in crate::native) fn require_events_directory(directory: &Path) -> Result<()> {
    match events_directory_state(directory)? {
        EventsDirectory::Present => Ok(()),
        EventsDirectory::Missing => bail!("events directory is missing"),
    }
}

#[cfg(test)]
pub(in crate::native) fn write_event(directory: &Path, event: &SessionEvent) -> Result<()> {
    write_json_atomic(
        &directory
            .join(crate::native::session::EVENTS_DIRECTORY)
            .join(new_event_file_name()?),
        event,
    )
}

pub(in crate::native) fn new_event_file_name() -> Result<String> {
    Ok(format!(
        "event-{}-{}.json",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    ))
}

pub(in crate::native) fn valid_event_file_name(name: &str) -> bool {
    name.starts_with("event-")
        && name.ends_with(".json")
        && name.len() <= 128
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
}

pub(in crate::native) fn event_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let events = directory.join(crate::native::session::EVENTS_DIRECTORY);
    let mut paths = Vec::new();
    if !events.is_dir() {
        return Ok(paths);
    }
    for entry in fs::read_dir(events)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(valid_event_file_name)
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(unix)]
pub(in crate::native) fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
pub(in crate::native) fn set_private_directory_permissions(path: &Path) -> Result<()> {
    terminal::windows_set_private_permissions(path, true)
}

#[cfg(unix)]
pub(in crate::native) fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
pub(in crate::native) fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let mut path = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path.as_mut_ptr(),
            path.len() as u32,
            0,
        )
    };
    if length == 0 || length as usize >= path.len() {
        return Err(std::io::Error::last_os_error()).context("failed to resolve private file path");
    }
    path.truncate(length as usize);
    terminal::windows_set_private_permissions(&PathBuf::from(String::from_utf16(&path)?), false)
}

pub(in crate::native) fn create_directory(
    root: &Path,
    durable_directories: &[PathBuf],
) -> Result<(PathBuf, String, Option<String>)> {
    let ancestry_error = create_state_root(root, durable_directories)?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(root)?;
    let directory = temp.keep();
    set_private_directory_permissions(&directory)?;
    // The session directory is itself a record: sync the root so its entry survives a
    // crash the same way the files written inside it do.
    sync_directory(root)
        .with_context(|| format!("failed to sync state root {}", root.display()))?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("session directory name is not UTF-8")?
        .to_owned();
    require_valid_session_id(&id)?;
    let events = directory.join(crate::native::session::EVENTS_DIRECTORY);
    fs::create_dir(&events)?;
    set_private_directory_permissions(&events)?;
    sync_directory(&directory)
        .with_context(|| format!("failed to sync session directory {}", directory.display()))?;
    Ok((directory, id, ancestry_error))
}

pub(in crate::native) fn try_lock(directory: &Path) -> Result<Option<TurnClaimLock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(TURN_CLAIM_LOCK_FILE))?;
    set_private_file_permissions(&file)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(TurnClaimLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
