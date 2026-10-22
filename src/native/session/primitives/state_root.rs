use super::*;

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
