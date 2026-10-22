use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
#[cfg(any(test, windows))]
use std::path::PathBuf;
#[cfg(windows)]
use std::{thread, time::Duration};

#[cfg(test)]
use anyhow::bail;
use anyhow::{Context, Result};
use serde::Serialize;

#[cfg(all(test, windows))]
use super::close_io_test;
use crate::native::session::close;
#[cfg(windows)]
use crate::native::terminal;

pub(in crate::native) fn remove_file_if_present(path: &Path) -> Result<()> {
    close::budget::check()?;
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
    close::budget::check()?;
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

/// Retry record replacement for bounded Windows sharing/access errors: polling readers can block rename.
/// Other errors still fail; see AGENTS.md, Current Windows Surface Boundary.
pub(in crate::native) fn persist_record(
    temporary: tempfile::NamedTempFile,
    path: &Path,
) -> std::io::Result<File> {
    close::budget::check()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
        let deadline = crate::native::session::close::budget::close_now() + RECORD_REPLACE_WAIT;
        let deadline = close::budget::deadline().map_or(deadline, |close| deadline.min(close));
        let mut temporary = temporary;
        loop {
            close::budget::check()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?;
            match temporary.persist(path) {
                Ok(file) => return Ok(file),
                Err(error)
                    if crate::native::session::close::budget::close_now() < deadline
                        && error.error.raw_os_error().is_some_and(|code| {
                            [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION].contains(&(code as u32))
                        }) =>
                {
                    temporary = error.file;
                    #[cfg(test)]
                    close_io_test::at(close_io_test::Point::RecordReplaceRetry);
                    thread::sleep(Duration::from_millis(5).min(
                        deadline.saturating_duration_since(
                            crate::native::session::close::budget::close_now(),
                        ),
                    ));
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
