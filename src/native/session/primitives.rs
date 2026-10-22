//! Record I/O and durability; lifecycle decisions remain in native.
use super::*;

mod durable;
mod locks;
mod state_root;

// Preserve the existing namespace, including helpers now used only inside a child.
#[allow(unused_imports)]
pub(in crate::native) use durable::{
    SyncKind, fault_point, fault_point_keeping_temporary, persist_record, record_publication_read,
    record_sync, remove_file_if_present, rename_session_file, sync_directory,
    sync_directory_entries, sync_file, sync_parent_directory, write_json_atomic, write_private,
};
#[cfg(test)]
pub(in crate::native) use durable::{
    SyncRecord, injected_fault, injected_sync_failure, with_fault_budget,
    with_publication_read_log, with_sync_failure, with_sync_log,
};
#[cfg(any(unix, windows))]
pub(in crate::native) use durable::{
    set_private_directory_permissions, set_private_file_permissions,
};
pub(in crate::native) use locks::{
    StatusLock, TurnClaimLock, lock_status, lock_turn_claim, try_lock,
};
#[allow(unused_imports)]
pub(in crate::native) use state_root::{
    StateRootDurabilityReceipt, create_directory, create_state_root, default_state_root,
    home_directories, same_directory, session_directory, session_directory_in, state_root,
    state_root_durability_receipt_present, sync_state_root_ancestry, validate_hook_directory,
};

pub(in crate::native) const STATE_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_STATE_DIR";
pub(in crate::native) const SESSION_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_SESSION_DIR";

pub(in crate::native) fn valid_session_id(value: &str) -> bool {
    value.starts_with("session-")
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(in crate::native) fn require_valid_session_id(value: &str) -> Result<()> {
    if !valid_session_id(value) {
        bail!("invalid Agent Bridge session id: {value:?}");
    }
    Ok(())
}

// macOS repair deliberately retains a surface that may outlive its owner. That
// cleanup obligation must not turn a known process exit into a request timeout.
// This only diagnoses a dead PID: a reused/live PID or an unknown identity is not
// proof of death, and nothing here consumes the surface or the pending claim.
pub(in crate::native) fn unix_ms() -> u128 {
    #[cfg(test)]
    if let Some(now) = super::tests::FIXED_UNIX_MS.get() {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// The Request source recorded when the caller is not a managed session.
pub(in crate::native) const EXTERNAL_SOURCE: &str = "external";

pub(in crate::native) fn delegation_source() -> String {
    std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID").unwrap_or_else(|_| EXTERNAL_SOURCE.to_owned())
}

pub(in crate::native) fn is_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// `status.json` or `closed.json` when present: a regular file, decoded strictly.
pub(in crate::native) fn read_regular_status_if_present(
    path: &Path,
) -> Result<Option<SessionStatus>> {
    read_regular_bytes_if_present(path)?
        .map(|bytes| record::status(&bytes).map_err(|error| error.in_path(path)))
        .transpose()
}

/// The raw bytes of a required regular session file; a missing file is
/// `failed to read {path}` with its I/O cause.
pub(in crate::native) fn read_regular_bytes(path: &Path) -> Result<Vec<u8>> {
    read_regular_bytes_if_present(path)?.map_or_else(
        || {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                .with_context(|| format!("failed to read {}", path.display()))
        },
        Ok,
    )
}

pub(in crate::native) fn read_regular_text_if_present(path: &Path) -> Result<Option<String>> {
    Ok(read_regular_bytes_if_present(path)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// The raw bytes of a regular session file: what the core Record decoders in `record.rs`
/// take, and what a caller that must not alter an event decodes strictly instead of
/// through the Snapshot's lossy event read.
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
pub(in crate::native) fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    // The manifest decides a session's scope (workspace, provider) for every read-only
    // query, so it is read like every other session record: a link at `manifest.json`
    // would let content outside the state root steer a search or an attach, and is
    // refused rather than followed.
    let path = directory.join(crate::native::session::MANIFEST_FILE);
    let bytes = read_regular_bytes(&path)?;
    let expected_id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    record::manifest(&bytes, expected_id).map_err(|error| error.in_path(&path))
}

pub(in crate::native) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

// Observe the actual I/O boundary, not time spent reaching it. Test callbacks
// may synchronize a worker or advance its thread-local close clock.
#[cfg(test)]
pub(in crate::native) mod close_io_test {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::native) enum Point {
        BeforeLock(&'static str),
        BlockingLock,
        WouldBlock(&'static str),
        #[cfg(windows)]
        RecordReplaceRetry,
    }
    type Hook = Box<dyn FnMut(Point)>;
    thread_local! {
        static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
    }
    pub(in crate::native::session) fn at(point: Point) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(point);
            }
        });
    }
    pub(in crate::native) fn during<T>(
        hook: impl FnMut(Point) + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Reset(Option<Hook>);
        impl Drop for Reset {
            fn drop(&mut self) {
                HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _reset = Reset(HOOK.with(|slot| slot.replace(Some(Box::new(hook)))));
        run()
    }
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
