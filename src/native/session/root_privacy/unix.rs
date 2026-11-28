use super::{Action, Mismatch};
use anyhow::{Context, Result, bail};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};

#[cfg(target_os = "macos")]
mod macos;

pub(super) fn create_leaf(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn mode_remedy(path: &Path) -> String {
    format!(
        "restore owner permissions with chmod 700 '{}'",
        path.display().to_string().replace('\'', "'\\''")
    )
}

#[cfg(target_os = "macos")]
const PRIVACY_ACCESS: &str = "check the terminal's macOS privacy access to this directory";

// ACL and mode advice for a denied directory operation applies to EACCES on Darwin.
// EPERM can come from privacy controls even when both the mode and ACL allow access.
fn access_error(error: io::Error, advice: impl FnOnce() -> String) -> anyhow::Error {
    #[cfg(target_os = "macos")]
    match error.raw_os_error() {
        Some(libc::EPERM) => return anyhow::Error::new(error).context(PRIVACY_ACCESS),
        Some(libc::EACCES) => {}
        _ => return error.into(),
    }
    anyhow::Error::new(error).context(advice())
}

fn open(path: &Path, no_follow: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | if no_follow { libc::O_NOFOLLOW } else { 0 })
        .open(path)
}

// The owner of a direct directory can set its mode despite lacking read access.
// Do not change a link target by pathname. Verify the reopened directory's identity.
fn open_root(path: &Path, action: Action) -> Result<(File, bool)> {
    match open(path, false) {
        Ok(file) => Ok((file, false)),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            open_denied_root(path, action, error)
        }
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            Err(Mismatch("expected a directory; select a directory intended for TAB".into()).into())
        }
        Err(error) => Err(error).context("select an accessible directory intended for TAB"),
    }
}

fn open_denied_root(path: &Path, action: Action, error: io::Error) -> Result<(File, bool)> {
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::EPERM) {
        return Err(error).context(PRIVACY_ACCESS);
    }
    // A trailing slash (or "/.") makes lstat follow a final link.
    let path = path.components().as_path();
    let meta = fs::symlink_metadata(path).map_err(|error| access_error(error, || {
        let access = format!("failed to inspect state directory {}; check search access to its parent directories", path.display());
        #[cfg(target_os = "macos")]
        let access = format!("{access}; if the directory ACL denies access, {}", macos::acl_remedy(path));
        access
    }))?;
    if !meta.is_dir() {
        return Err(error).context(mode_remedy(path));
    }
    judge_owner(meta.uid(), unsafe { libc::geteuid() })?;
    #[cfg(target_os = "macos")]
    if meta.mode() & 0o700 == 0o700 {
        return Err(error).with_context(|| {
            format!(
                "owner mode permits access; check for a denying ACL entry and {}",
                macos::acl_remedy(path)
            )
        });
    }
    if action == Action::Inspect {
        return Err(error).context(mode_remedy(path));
    }
    let name = CString::new(path.as_os_str().as_bytes())?;
    if unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            name.as_ptr(),
            0o700,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EPERM) {
            return Err(error).context(PRIVACY_ACCESS);
        }
        return Err(error).context(mode_remedy(path));
    }
    let file = open(path, true).map_err(|error| access_error(error, || {
        #[cfg(target_os = "macos")]
        { format!("cannot reopen after restoring owner mode; check for a denying ACL entry and {}", macos::acl_remedy(path)) }
        #[cfg(not(target_os = "macos"))]
        { mode_remedy(path) }
    }))?;
    let now = file.metadata()?;
    if (meta.dev(), meta.ino()) != (now.dev(), now.ino()) {
        bail!("directory changed during mode repair; verify the directory and retry");
    }
    Ok((file, meta.mode() & 0o7777 != 0o700))
}

fn judge_owner(owner: u32, user: u32) -> Result<()> {
    if owner != user {
        return Err(Mismatch(format!("owner uid {owner}, expected {user}; select a TAB directory owned by uid {user} or ask its owner to correct ownership")).into());
    }
    Ok(())
}

fn judge_mode(mode: u32, path: &Path) -> Result<()> {
    if mode != 0o700 {
        return Err(Mismatch(format!(
            "mode {mode:04o}, expected 0700; {}",
            mode_remedy(path)
        ))
        .into());
    }
    Ok(())
}

struct Permissions {
    owner: u32,
    mode: u32,
    #[cfg(target_os = "macos")]
    acl: io::Result<Option<String>>,
}

impl Permissions {
    fn read(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            owner: meta.uid(),
            mode: meta.mode() & 0o7777,
            #[cfg(target_os = "macos")]
            acl: macos::first_acl_entry(file),
        })
    }

    #[cfg(target_os = "macos")]
    fn acl_observation(&self) -> Option<String> {
        match &self.acl {
            Ok(entry) => entry.clone(),
            Err(error) => Some(format!("cannot read extended ACL ({error})")),
        }
    }

    fn judge(&self, user: u32, path: &Path) -> Result<()> {
        judge_owner(self.owner, user)?;
        #[cfg(target_os = "macos")]
        if let Some(observed) = self.acl_observation() {
            match &self.acl {
                Ok(_) => {
                    return Err(Mismatch(format!("{observed}; {}", macos::acl_remedy(path))).into());
                }
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                    bail!("{observed}; {PRIVACY_ACCESS}")
                }
                Err(e) if e.raw_os_error() == Some(libc::EACCES) => {
                    bail!("{observed}; {}", macos::acl_remedy(path))
                }
                Err(_) => bail!("{observed}"),
            }
        }
        judge_mode(self.mode, path)
    }

    fn repair(&self, file: &File, path: &Path, _created: bool, issue: anyhow::Error) -> Result<()> {
        #[cfg(target_os = "macos")]
        if let Some(observed) = self.acl_observation() {
            if self
                .acl
                .as_ref()
                .is_err_and(|e| e.raw_os_error() != Some(libc::EACCES))
            {
                return Err(issue);
            }
            if !_created
                && !macos::acl_removal_allowed(file, self.mode).map_err(|e| {
                    let detail = if e.raw_os_error() == Some(libc::EPERM) {
                        format!("{observed}; {PRIVACY_ACCESS}")
                    } else {
                        issue.to_string()
                    };
                    anyhow::Error::new(e).context(detail)
                })?
            {
                return Err(super::NoTabEntry(observed).into());
            }
            return macos::clear_acl(file).map_err(|e| {
                // An immutable flag is an observable reason for the setter's EPERM.
                // Otherwise it can be a privacy denial, even after a successful read.
                if e.raw_os_error() == Some(libc::EPERM) && !macos::is_immutable(file) {
                    return anyhow::Error::new(e).context(format!(
                        "{observed}; failed to remove the extended ACL; {PRIVACY_ACCESS}"
                    ));
                }
                anyhow::Error::new(e)
                    .context("failed to remove the extended ACL")
                    .context(issue)
            });
        }
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .with_context(|| mode_remedy(path))
            .context(issue)
    }
}

pub(super) fn check(path: &Path, action: Action) -> Result<bool> {
    let (file, opened_with_repair) = open_root(path, action)?;
    let user = unsafe { libc::geteuid() };
    let before = Permissions::read(&file)?;
    let Err(issue) = before.judge(user, path) else {
        return Ok(opened_with_repair);
    };
    let Action::Prepare { created } = action else {
        return Err(issue);
    };
    if before.owner != user {
        return Err(issue);
    }
    before.repair(&file, path, created, issue)?;
    Permissions::read(&file)?.judge(user, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_creation_refuses_a_foreign_owned_directory_without_changing_it() {
        let user = unsafe { libc::geteuid() };
        if user == 0 {
            eprintln!("skipping / fixture: requires non-root effective uid");
            return;
        }
        let root = Path::new("/");
        let before = fs::metadata(root).unwrap();
        if before.uid() != 0 {
            eprintln!(
                "skipping / fixture: expected owner uid 0, observed {}",
                before.uid()
            );
            return;
        }
        let error = crate::native::session::primitives::create_state_root(root, &[]).unwrap_err();
        let expected = format!(
            "owner uid 0, expected {user}; select a TAB directory owned by uid {user} or ask its owner to correct ownership"
        );
        assert!(
            error.chain().any(|e| e.to_string() == expected),
            "{error:#}"
        );
        let after = fs::metadata(root).unwrap();
        assert_eq!((after.uid(), after.mode()), (before.uid(), before.mode()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_open_eperm_preserves_mode_and_reports_privacy_access() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        for action in [Action::Inspect, Action::Prepare { created: false }] {
            let denied = io::Error::from_raw_os_error(libc::EPERM);
            let expected =
                format!("check the terminal's macOS privacy access to this directory: {denied}");
            let error = open_denied_root(root.path(), action, denied).unwrap_err();
            assert_eq!(format!("{error:#}"), expected);
            assert_eq!(fs::metadata(root.path()).unwrap().mode() & 0o7777, 0o750);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_acl_read_eperm_reports_terminal_privacy_access() {
        let error = io::Error::from_raw_os_error(libc::EPERM);
        let expected = format!(
            "cannot read extended ACL ({error}); check the terminal's macOS privacy access to this directory"
        );
        let permissions = Permissions {
            owner: 42,
            mode: 0o700,
            acl: Err(error),
        };
        assert_eq!(
            permissions
                .judge(42, Path::new("tab"))
                .unwrap_err()
                .to_string(),
            expected
        );
    }

    #[test]
    fn foreign_uid_and_fixed_nonprivate_mode_fail_the_readback_judgment() {
        let owner = judge_owner(42, 13).unwrap_err();
        assert_eq!(
            owner.to_string(),
            "owner uid 42, expected 13; select a TAB directory owned by uid 13 or ask its owner to correct ownership"
        );
        let path = Path::new("tab");
        assert_eq!(
            judge_mode(0o755, path).unwrap_err().to_string(),
            "mode 0755, expected 0700; restore owner permissions with chmod 700 'tab'"
        );
        judge_mode(0o700, path).unwrap();
    }
}
