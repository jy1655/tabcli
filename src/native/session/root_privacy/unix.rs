use super::{Action, Cause, Entries, Flags, Observation, tab_entry};
use anyhow::{Context, Result};
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

mod entries;
use entries::has_tab_entry;
#[cfg(target_os = "macos")]
mod macos;

pub(super) fn create_leaf(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn flags(_meta: &fs::Metadata) -> Flags {
    #[cfg(target_os = "macos")]
    {
        use std::os::darwin::fs::MetadataExt;
        if _meta.st_flags() & libc::SF_IMMUTABLE != 0 {
            return Flags::SystemImmutable;
        }
        if _meta.st_flags() & libc::UF_IMMUTABLE != 0 {
            return Flags::UserImmutable;
        }
    }
    Flags::None
}

fn open(path: &Path, no_follow: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | if no_follow { libc::O_NOFOLLOW } else { 0 })
        .open(path)
}

fn open_root(path: &Path, action: Action) -> Result<(File, bool)> {
    match open(path, false) {
        Ok(file) => Ok((file, false)),
        Err(error)
            if matches!(
                Cause::of(&error),
                Cause::AccessDenied | Cause::PermissionDenied | Cause::NotPermitted
            ) =>
        {
            open_denied_root(path, action, error)
        }
        Err(error) => Err(action.open_failure(path, error)),
    }
}

fn open_denied_root(path: &Path, action: Action, error: io::Error) -> Result<(File, bool)> {
    let entries = action.unopened();
    #[cfg(target_os = "macos")]
    if Cause::of(&error) == Cause::NotPermitted {
        return Err(Observation::Open.render(path, &entries, Flags::None, Some(error.into())));
    }
    // A trailing slash (or "/.") makes lstat follow a final link.
    let path = path.components().as_path();
    let meta = fs::symlink_metadata(path).map_err(|e| {
        Observation::InspectPath.render(path, &entries, Flags::None, Some(e.into()))
    })?;
    let flags = flags(&meta);
    if !meta.is_dir() {
        return Err(Observation::Open.render(path, &entries, flags, Some(error.into())));
    }
    let user = unsafe { libc::geteuid() };
    if meta.uid() != user {
        return Err(Observation::Owner {
            actual: meta.uid(),
            expected: user,
        }
        .render(path, &entries, flags, None));
    }
    let mode = Observation::Mode(meta.mode() & 0o7777);
    #[cfg(target_os = "macos")]
    if meta.mode() & 0o700 == 0o700 {
        return Err(Observation::OpenWithOwnerAccess.render(
            path,
            &entries,
            flags,
            Some(error.into()),
        ));
    }
    if action == Action::Inspect {
        return Err(mode.render(path, &entries, flags, Some(error.into())));
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
        let failure =
            anyhow::Error::new(io::Error::last_os_error()).context("fchmodat: restore owner mode");
        return Err(mode.render(path, &entries, flags, Some(failure)));
    }
    let file = open(path, true)
        .map_err(|e| Observation::Reopen.render(path, &entries, flags, Some(e.into())))?;
    let now = file.metadata()?;
    if (meta.dev(), meta.ino()) != (now.dev(), now.ino()) {
        return Err(Observation::Replaced.render(
            path,
            &Entries::Undetermined(None),
            Flags::None,
            None,
        ));
    }
    Ok((file, meta.mode() & 0o7777 != 0o700))
}

struct Permissions {
    owner: u32,
    mode: u32,
    flags: Flags,
    #[cfg(target_os = "macos")]
    acl: io::Result<Option<String>>,
}

impl Permissions {
    fn read(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            owner: meta.uid(),
            mode: meta.mode() & 0o7777,
            flags: flags(&meta),
            #[cfg(target_os = "macos")]
            acl: macos::first_acl_entry(file),
        })
    }

    fn judge(self, user: u32) -> Option<Observation> {
        if self.owner != user {
            return Some(Observation::Owner {
                actual: self.owner,
                expected: user,
            });
        }
        #[cfg(target_os = "macos")]
        match self.acl {
            Ok(Some(entry)) => return Some(Observation::Acl(entry)),
            Err(error) => return Some(Observation::AclRead(error)),
            Ok(None) => {}
        }
        (self.mode != 0o700).then_some(Observation::Mode(self.mode))
    }
}

pub(super) fn check(path: &Path, action: Action) -> Result<bool> {
    let (file, opened_with_repair) = open_root(path, action)?;
    let entries = || action.entries(|| has_tab_entry(&file).context("inspect TAB entries"));
    let read = || {
        Permissions::read(&file).map_err(|error| {
            Observation::Read.render(path, &entries(), Flags::None, Some(error.into()))
        })
    };
    let user = unsafe { libc::geteuid() };
    let before = read()?;
    let observed_flags = before.flags;
    // A flag can change between judgment and a refused setter.
    #[cfg(target_os = "macos")]
    let repair_flags = || file.metadata().map_or(observed_flags, |meta| flags(&meta));
    #[cfg(not(target_os = "macos"))]
    let repair_flags = || observed_flags;
    #[cfg(target_os = "macos")]
    let mode = before.mode;
    let Some(issue) = before.judge(user) else {
        return Ok(opened_with_repair);
    };
    if action == Action::Inspect || matches!(issue, Observation::Owner { .. }) {
        return Err(issue.render(path, &entries(), observed_flags, None));
    }

    match &issue {
        #[cfg(target_os = "macos")]
        Observation::AclRead(error) if Cause::of(error) != Cause::AccessDenied => {
            return Err(issue.render(path, &entries(), observed_flags, None));
        }
        #[cfg(target_os = "macos")]
        Observation::Acl(_) | Observation::AclRead(_) => {
            // Only preparation may restore search permission to establish eligibility.
            if action == (Action::Prepare { created: false }) && mode != 0o700 {
                file.set_permissions(fs::Permissions::from_mode(0o700))
                    .map_err(|error| {
                        issue.render(
                            path,
                            &entries(),
                            repair_flags(),
                            Some(
                                anyhow::Error::new(error)
                                    .context("restore mode before checking for a TAB entry"),
                            ),
                        )
                    })?;
            }
            let origin = entries();
            if !matches!(origin, Entries::Known) {
                return Err(issue.render(path, origin.required_for_repair(), observed_flags, None));
            }
            macos::clear_acl(&file).map_err(|error| {
                issue.render(
                    path,
                    &origin,
                    repair_flags(),
                    Some(anyhow::Error::new(error).context("failed to remove the extended ACL")),
                )
            })?;
        }
        Observation::Mode(_) => file
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|error| {
                issue.render(
                    path,
                    &entries(),
                    repair_flags(),
                    Some(anyhow::Error::new(error).context("fchmod: restore owner mode")),
                )
            })?,
        _ => unreachable!("only a mode or ACL mismatch can be repaired"),
    }
    let after = read()?;
    let flags = after.flags;
    if let Some(issue) = after.judge(user) {
        return Err(issue.render(path, &entries(), flags, None));
    }
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
            "owner uid 0, expected {user}; the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory); set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB"
        );
        if has_tab_entry(&File::open(root).unwrap()).unwrap() {
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "state root /: owner uid 0, expected {user}; select a TAB directory owned by uid {user}"
                )
            );
        } else {
            assert!(
                error.chain().any(|e| e.to_string() == expected),
                "{error:#}"
            );
        }
        let after = fs::metadata(root).unwrap();
        assert_eq!((after.uid(), after.mode()), (before.uid(), before.mode()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_open_eperm_reports_terminal_privacy_access_when_entries_are_unknown() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        for action in [Action::Inspect, Action::Prepare { created: false }] {
            let denied = io::Error::from_raw_os_error(libc::EPERM);
            let expected = format!(
                "open state directory; macOS privacy controls may prevent access; cannot determine whether the directory holds a TAB entry; check the terminal's macOS privacy access to this directory: {denied}"
            );
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
            "cannot read extended ACL ({error}); macOS privacy controls may prevent access; check the terminal's macOS privacy access to this directory"
        );
        let permissions = Permissions {
            owner: 42,
            mode: 0o700,
            flags: Flags::None,
            acl: Err(error),
        };
        assert_eq!(
            permissions
                .judge(42)
                .unwrap()
                .render(Path::new("tab"), &Entries::Known, Flags::None, None)
                .to_string(),
            expected
        );
    }

    #[test]
    fn foreign_uid_and_fixed_nonprivate_mode_fail_the_readback_judgment() {
        let permissions = |owner, mode| Permissions {
            owner,
            mode,
            flags: Flags::None,
            #[cfg(target_os = "macos")]
            acl: Ok(None),
        };
        let path = Path::new("tab");
        let rendered = |issue: Observation| {
            issue
                .render(path, &Entries::Known, Flags::None, None)
                .to_string()
        };
        assert_eq!(
            rendered(permissions(42, 0o755).judge(13).unwrap()),
            "owner uid 42, expected 13; select a TAB directory owned by uid 13"
        );
        assert_eq!(
            rendered(permissions(13, 0o755).judge(13).unwrap()),
            "mode 0755, expected 0700; restore owner permissions with chmod 700 'tab'"
        );
        assert!(permissions(13, 0o700).judge(13).is_none());
    }
}
