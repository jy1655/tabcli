use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::native::session::root_privacy::{self, FailurePolicy};
use crate::native::session_start::deadline;

use super::durable::{
    fault_point, set_private_directory_permissions, sync_directory, write_json_atomic,
};
use super::{STATE_DIR_ENV, require_valid_session_id, unix_ms};
use crate::native::session::names::{STATE_ROOT_ANCESTRY_SYNC_LIMIT, STATE_ROOT_DURABLE_FILE};

/// The receipt stored at `STATE_ROOT_DURABLE_FILE`. Only its existence as a regular file
/// carries meaning; the fields describe the walk that wrote it.
#[derive(Serialize)]
pub(in crate::native) struct StateRootDurabilityReceipt {
    schema: u32,
    synced_unix_ms: u128,
}

pub(in crate::native) fn create_root_directories(
    root: &Path,
    root_fact: &'static str,
    policy: FailurePolicy,
) -> Result<bool> {
    let mut created = Vec::new();
    let mut probe = root;
    loop {
        match fs::symlink_metadata(probe) {
            Ok(_) => break,
            // Darwin can deny path stat (readsecurity) while a read-open succeeds.
            // Let the opened-root check judge this leaf before any child write.
            #[cfg(target_os = "macos")]
            Err(error) if probe == root && error.kind() == std::io::ErrorKind::PermissionDenied => {
                break;
            }
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
    let mut made_root = false;
    for directory in created.iter().rev() {
        // A concurrent creator may win the race; the ancestry walk below covers its
        // entries and this creator's alike.
        deadline::before("creating state directories")?;
        let leaf = directory == root;
        let result = if leaf {
            root_privacy::create_leaf(directory, policy)
        } else {
            fs::create_dir(directory)
        };
        match result {
            Ok(()) if leaf => {
                made_root = true;
                deadline::wrote(root_fact);
            }
            Ok(()) => deadline::wrote("state directories were created"),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create state directory {}", directory.display())
                });
            }
        }
    }
    Ok(made_root)
}

/// Establishes root privacy before any receipt or Session write, then makes its ancestry
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
/// Expiry during the walk ends creation. Other walk failures are returned for the launch
/// status and leave the receipt absent so the next creation walks again.
pub(in crate::native) fn create_state_root(
    root: &Path,
    durable_directories: &[PathBuf],
) -> Result<Option<String>> {
    create_state_root_with(root, durable_directories, FailurePolicy::Refuse)
}

pub(in crate::native) fn create_state_root_with(
    root: &Path,
    durable_directories: &[PathBuf],
    policy: FailurePolicy,
) -> Result<Option<String>> {
    let created = create_root_directories(root, "state directories were created", policy)?;
    root_privacy::prepare(root, created, policy)?;
    if state_root_durability_receipt_present(root) {
        return Ok(None);
    }
    fault_point("syncing the state root's ancestry")?;
    if let Err(error) = sync_state_root_ancestry(root, durable_directories) {
        if error.is::<crate::native::failure_kind::TimedOut>() {
            return Err(error);
        }
        return Ok(Some(format!(
            "state root ancestry was not made durable: {error:#}"
        )));
    }
    let receipt = StateRootDurabilityReceipt {
        schema: 1,
        synced_unix_ms: unix_ms(),
    };
    deadline::before("writing the state root durability receipt")?;
    write_json_atomic(&root.join(STATE_ROOT_DURABLE_FILE), &receipt)?;
    deadline::wrote("the state root durability receipt was written");
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
/// entries are not TAB's to establish, or after `STATE_ROOT_ANCESTRY_SYNC_LIMIT`
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
        deadline::before("syncing state root ancestry")?;
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
    let metadata =
        fs::symlink_metadata(&directory).with_context(|| format!("no such TAB session: {id}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("refusing non-directory TAB session: {id}");
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
    deadline::before("creating the Session directory")?;
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

#[cfg(test)]
mod request_deadline_tests {
    use super::*;
    use crate::native::session_start::deadline;

    #[test]
    fn request_deadline_state_root_directory_is_absent() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("new");
        let result = deadline::expire_at("creating state directories", || {
            create_state_root(&root, &[])
        });
        assert!(!root.exists(), "state root was created");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn request_deadline_existing_state_root_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let result = deadline::during(std::time::Instant::now(), || {
            create_state_root(root.path(), &[])
        });
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("syncing state root ancestry"),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("state root permissions were set"),
            "{error:#}"
        );
        assert!(!state_root_durability_receipt_present(root.path()));
    }

    #[test]
    fn request_deadline_existing_private_root_reports_no_change() {
        let root = tempfile::tempdir().unwrap();
        create_state_root(root.path(), &[]).unwrap();
        let error = deadline::expire_at("creating the Session directory", || {
            create_directory(root.path(), &[])
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the request's deadline passed before creating the Session directory; no Session was created"
        );
    }

    #[test]
    fn request_deadline_state_root_receipt_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let result = deadline::expire_at("writing the state root durability receipt", || {
            create_state_root(root.path(), &[])
        });
        assert!(
            !state_root_durability_receipt_present(root.path()),
            "receipt was written"
        );
        assert!(result.is_err());
    }

    #[test]
    fn request_deadline_session_directory_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let result = deadline::expire_at("creating the Session directory", || {
            create_directory(root.path(), &[])
        });
        assert!(state_root_durability_receipt_present(root.path()));
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "Session directory was created"
        );
        assert!(result.is_err());
    }
}

#[cfg(all(test, unix))]
mod privacy_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn unreadable_link_target_requires_owner_mode_repair() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        let link = parent.path().join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
        let result = create_state_root(&link, &[]);
        let after = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "creation repaired an unreadable linked root"
        );
        assert_eq!(after, 0);
    }

    #[test]
    fn direct_owner_root_with_no_read_mode_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o000)).unwrap();
        create_state_root(root.path(), &[]).unwrap();
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn immutable_acl_names_the_entry_and_remedy_before_retry() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        fs::create_dir(path.join("workspace-consent")).unwrap();
        let apply = |tool: &str, args: &[&str]| {
            let output = Command::new(tool).args(args).arg(path).output().unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        apply(
            "/bin/chmod",
            &["+a", "everyone allow add_file,delete_child"],
        );
        apply("chflags", &["uchg"]);
        let result = create_directory(path, &[]);
        apply("chflags", &["nouchg"]);
        let error = result.unwrap_err();
        let reason = error
            .chain()
            .map(ToString::to_string)
            .find(|reason| reason.starts_with("ACL entry 0 (allow, principal "))
            .unwrap();
        assert!(
            reason.ends_with(&format!("); run chmod -N '{}' and retry", path.display())),
            "{reason}"
        );
        assert_eq!(fs::read_dir(path).unwrap().count(), 1);
        create_directory(path, &[]).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn acl_removal_requires_a_tab_entry_in_an_existing_directory() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        let status = Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            create_directory(path, &[]).is_err(),
            "unrecognized directory was used for a Session"
        );
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
        fs::create_dir(path.join("workspace-consent")).unwrap();
        create_directory(path, &[]).unwrap();
        let listing = Command::new("/bin/ls")
            .arg("-de")
            .arg(path)
            .output()
            .unwrap();
        assert!(listing.status.success());
        assert!(
            !listing
                .stdout
                .split(|b| *b == b'\n')
                .any(|line| line.starts_with(b" 0:"))
        );
    }
}

#[cfg(all(test, windows))]
mod windows_privacy_tests;
