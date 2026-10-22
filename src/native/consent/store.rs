#[cfg(unix)]
use crate::native::private_to_user::owned_and_not_writable_by_others;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(windows)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct Identity {
    pub(in crate::native) path: PathBuf,
    volume: u64,
    file: u64,
    created: u128,
    pub(super) owner: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct Evidence {
    pub(in crate::native) provider: String,
    pub(in crate::native) store: PathBuf,
    pub(in crate::native) key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::native) enum Trust {
    Trusted(Evidence),
    Absent,
    Declined,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct LaunchTrust {
    pub(in crate::native) answers_launch_dialog: bool,
    pub(in crate::native) bypass_suppresses_dialog: bool,
    pub(in crate::native) applied_marker: Option<&'static str>,
    pub(in crate::native) dialog_settle: Duration,
}

// A raw snapshot of the caller's environment variables that locate a provider's home.
// It holds no path and validates nothing: each provider adapter resolves its own home
// from it with its own spelling, and each boundary applies its own requirement (the
// trust and admission boundaries need a user home, see `user_home`; the Claude session
// registry and the Agy brain root keep their own rules).
#[derive(Clone, Debug, Default)]
pub(in crate::native) struct Environment {
    pub(in crate::native) home: Option<OsString>,
    pub(in crate::native) userprofile: Option<OsString>,
    pub(in crate::native) codex_home: Option<OsString>,
    pub(in crate::native) claude_config_dir: Option<OsString>,
    pub(in crate::native) pi_coding_agent_dir: Option<OsString>,
}

impl Environment {
    pub(in crate::native) fn current() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            userprofile: std::env::var_os("USERPROFILE"),
            codex_home: std::env::var_os("CODEX_HOME"),
            claude_config_dir: std::env::var_os("CLAUDE_CONFIG_DIR"),
            pi_coding_agent_dir: std::env::var_os("PI_CODING_AGENT_DIR"),
        }
    }

    // A caller whose only variable is `HOME`, as the tests point the provider homes at a
    // temporary directory.
    #[cfg(test)]
    pub(in crate::native) fn at_home(home: &Path) -> Self {
        Self {
            home: Some(home.as_os_str().to_owned()),
            ..Self::default()
        }
    }
}

// The user home that workspace trust and the launch admission require: `HOME` when set,
// even empty, otherwise `USERPROFILE`. A provider override or a recorded source store
// does not replace it (tests/launch_admission.rs pins this for an empty `CODEX_HOME`).
pub(in crate::native) fn user_home(env: &Environment) -> Result<&OsStr> {
    env.home
        .as_deref()
        .or(env.userprofile.as_deref())
        .context("no user home for workspace trust lookup")
}

pub(in crate::native) fn identity(path: &Path) -> Result<Identity> {
    let path = path.canonicalize().context("cannot identify workspace")?;
    let meta = fs::metadata(&path)?;
    if !meta.is_dir() {
        bail!("workspace is not a directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Identity {
            path,
            volume: meta.dev(),
            file: meta.ino(),
            created: meta
                .created()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |t| t.as_nanos()),
            owner: meta.uid().to_string(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
        };
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot identify workspace directory handle");
        }
        Ok(Identity {
            owner: windows_owner(&path)?,
            path,
            volume: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            created: (u128::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u128::from(info.ftCreationTime.dwLowDateTime),
        })
    }
}

// A provider key may use the ordinary Windows spelling, but consent never compares
// provider strings as directory identities. Unix spelling remains case-sensitive.
pub(in crate::native) fn native_key(path: &Path) -> Result<String> {
    let text = path.to_str().context("workspace path is not UTF-8")?;
    #[cfg(windows)]
    return Ok(text
        .strip_prefix(r"\\?\UNC\")
        .map(|p| format!(r"\\{p}"))
        .unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()));
    #[cfg(not(windows))]
    Ok(text.to_owned())
}

pub(in crate::native) fn read_store(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        bail!("trust store is not a regular file");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let opened_meta = file.metadata()?;
    #[cfg(unix)]
    {
        if !owned_and_not_writable_by_others(&opened_meta, unsafe { libc::geteuid() }) {
            bail!("trust store is not owned and writable only by the current user");
        }
    }
    // Both are read from the handle that the content is read from, so that they
    // describe the same file.
    #[cfg(windows)]
    {
        let (owner, writers) = windows_store_access(&file)?;
        windows_store_is_private(&owner, &writers, &windows_current_owners()?)?;
    }
    if !opened_meta.is_file() || opened_meta.len() > 8 * 1024 * 1024 {
        bail!("trust store exceeds 8 MiB");
    }
    let mut text = String::new();
    file.take(8 * 1024 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 8 * 1024 * 1024 {
        bail!("trust store exceeds 8 MiB");
    }
    Ok(Some(text))
}

/// The Windows counterpart of the Unix owner and mode check: a trust store counts as this
/// user's own only when one of this process's own owners owns it and no access rule lets
/// another account change it. `writers` are the accounts that such a rule names.
///
/// The system, the Administrators group and the owner (`OWNER RIGHTS`) may be among them:
/// they can change any file of this user whatever its access list says. Every other
/// account, such as `Authenticated Users` on a folder outside the user profile, could put
/// a workspace into the store that this user never approved.
#[cfg(windows)]
fn windows_store_is_private(owner: &str, writers: &[String], own: &[String]) -> Result<()> {
    const SYSTEM_WRITERS: [&str; 3] = ["S-1-5-18", "S-1-5-32-544", "S-1-3-4"];
    if !own.iter().any(|sid| sid == owner) {
        bail!("trust store owner differs from current user");
    }
    if let Some(other) = writers
        .iter()
        .find(|sid| !own.contains(sid) && !SYSTEM_WRITERS.contains(&sid.as_str()))
    {
        bail!("trust store can be changed by another account ({other})");
    }
    Ok(())
}

/// The owner of an open trust store, and every account that an access rule of the store
/// allows to change its content or its access list.
///
/// A store without an access list is open to everyone, and a rule of a kind other than a
/// plain allow or deny cannot be evaluated here; both are refused. A deny rule only takes
/// access away, and an inherit-only rule does not apply to the store itself.
#[cfg(windows)]
fn windows_store_access(file: &File) -> Result<(String, Vec<String>)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_ALL, GENERIC_WRITE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GetAce, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION,
        },
        Storage::FileSystem::{FILE_APPEND_DATA, FILE_WRITE_DATA, WRITE_DAC, WRITE_OWNER},
    };
    // `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE` of `winnt.h`.
    const ALLOWED: u8 = 0;
    const DENIED: u8 = 1;
    const CHANGE: u32 =
        FILE_WRITE_DATA | FILE_APPEND_DATA | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;

    let mut owner = std::ptr::null_mut();
    let mut access_list: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut access_list,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32))
            .context("cannot read the trust store's owner and access list");
    }
    let result = (|| {
        let owner = windows_sid(owner)?;
        if access_list.is_null() {
            bail!("trust store has no access list, so every account can change it");
        }
        let mut writers = Vec::new();
        for index in 0..u32::from(unsafe { (*access_list).AceCount }) {
            let mut rule = std::ptr::null_mut();
            if unsafe { GetAce(access_list, index, &mut rule) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot read an access rule of the trust store");
            }
            let header = unsafe { *rule.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || header.AceType == DENIED {
                continue;
            }
            if header.AceType != ALLOWED
                || usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            {
                bail!("trust store has an access rule that cannot be evaluated");
            }
            let allowed = rule.cast::<ACCESS_ALLOWED_ACE>();
            if unsafe { (*allowed).Mask } & CHANGE != 0 {
                let sid =
                    windows_sid(unsafe { std::ptr::addr_of_mut!((*allowed).SidStart) }.cast())?;
                if !writers.contains(&sid) {
                    writers.push(sid);
                }
            }
        }
        Ok((owner, writers))
    })();
    unsafe { LocalFree(descriptor) };
    result
}

// The owners that a file written by this user's own processes can have: the user, and the
// default owner of this process's token. For an elevated process that is the
// Administrators group, so a provider that ran elevated left its trust store owned by
// that group, and the files that a job on a GitHub Windows runner writes are owned by it.
// A store with any other owner was written by someone else.
#[cfg(windows)]
fn windows_current_owners() -> Result<Vec<String>> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Security::{GetTokenInformation, PSID, TOKEN_QUERY, TokenOwner, TokenUser},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let _handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
    let mut owners = Vec::new();
    // `TOKEN_USER` and `TOKEN_OWNER` both begin with the pointer to their SID.
    for class in [TokenUser, TokenOwner] {
        let mut needed = 0;
        unsafe { GetTokenInformation(raw, class, std::ptr::null_mut(), 0, &mut needed) };
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(raw, class, buffer.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let sid = windows_sid(unsafe { *buffer.as_ptr().cast::<PSID>() })?;
        if !owners.contains(&sid) {
            owners.push(sid);
        }
    }
    Ok(owners)
}

#[cfg(windows)]
fn windows_owner(path: &Path) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            OWNER_SECURITY_INFORMATION,
        },
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32).into());
    }
    let result = windows_sid(owner);
    unsafe { LocalFree(descriptor) };
    result
}

#[cfg(windows)]
fn windows_sid(sid: windows_sys::Win32::Security::PSID) -> Result<String> {
    use windows_sys::Win32::{
        Foundation::LocalFree, Security::Authorization::ConvertSidToStringSidW,
    };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let result =
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).map_err(Into::into);
    unsafe { LocalFree(text.cast()) };
    result
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::super::tests::{setup, trust_pi};
    use super::*;

    // Replaces the access list of a fixture with the one that `sddl` describes.
    #[cfg(windows)]
    fn set_access_list(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                    SE_FILE_OBJECT, SetNamedSecurityInfoW,
                },
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
                PROTECTED_DACL_SECURITY_INFORMATION,
            },
        };
        let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let (sddl_text, path_text) = (wide(sddl.as_ref()), wide(path.as_os_str()));
        let mut descriptor = std::ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{sddl}: {}", std::io::Error::last_os_error());
        let (mut present, mut defaulted) = (0, 0);
        let mut access_list = std::ptr::null_mut();
        let extracted = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut access_list, &mut defaulted)
        };
        assert!(extracted != 0 && present != 0, "{sddl}");
        let status = unsafe {
            SetNamedSecurityInfoW(
                path_text.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                access_list,
                std::ptr::null_mut(),
            )
        };
        unsafe { LocalFree(descriptor) };
        assert_eq!(status, 0, "{sddl}");
    }
    // A provider that ran elevated leaves its store owned by the Administrators group,
    // which is the default owner of an elevated token; the files that a job on a GitHub
    // Windows runner writes have that owner. Whichever owner Windows gives a file that
    // this process writes, it is one of this process's own.
    #[cfg(windows)]
    #[test]
    fn a_store_written_by_this_user_is_read_whoever_windows_made_its_owner() {
        let (_temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);

        let owners = windows_current_owners().unwrap();
        assert!((1..=2).contains(&owners.len()), "{owners:?}");
        assert!(
            owners.contains(&windows_owner(&fixture.pi).unwrap()),
            "{owners:?}"
        );
        assert!(read_store(&fixture.pi).unwrap().is_some());
    }
    // The owner alone does not say who can change a store: outside the user profile a
    // file usually inherits a rule that lets every signed-in account modify it.
    #[cfg(windows)]
    #[test]
    fn a_store_that_another_account_can_change_is_refused() {
        let (_temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        let user = windows_current_owners().unwrap().remove(0);
        let refusal = |sddl: String| {
            set_access_list(&fixture.pi, &sddl);
            read_store(&fixture.pi).unwrap_err().to_string()
        };

        // Everyone may write the content.
        assert_eq!(
            refusal(format!("D:P(A;;FA;;;{user})(A;;FW;;;WD)")),
            "trust store can be changed by another account (S-1-1-0)"
        );
        // Authenticated Users may rewrite the access list, and so give themselves the rest.
        assert_eq!(
            refusal(format!("D:P(A;;FA;;;{user})(A;;WD;;;AU)")),
            "trust store can be changed by another account (S-1-5-11)"
        );
        // Appending is enough to add a workspace, and a new owner can do everything. The
        // account need not exist: the rule is what counts.
        for rights in ["0x4", "WO"] {
            assert_eq!(
                refusal(format!(
                    "D:P(A;;FA;;;{user})(A;;{rights};;;S-1-5-21-1-2-3-4444)"
                )),
                "trust store can be changed by another account (S-1-5-21-1-2-3-4444)",
                "{rights}"
            );
        }
        assert_eq!(
            refusal("D:NO_ACCESS_CONTROL".to_owned()),
            "trust store has no access list, so every account can change it"
        );
        // A conditional rule grants what its condition decides, which is not evaluated.
        assert_eq!(
            refusal(format!(
                "D:P(A;;FA;;;{user})(XA;;FR;;;WD;(@User.Title==\"PM\"))"
            )),
            "trust store has an access rule that cannot be evaluated"
        );
    }
    #[cfg(windows)]
    #[test]
    fn a_store_that_others_can_only_read_stays_this_users_own() {
        let (_temp, workspace, fixture) = setup();
        trust_pi(&fixture, &workspace);
        let user = windows_current_owners().unwrap().remove(0);

        for sddl in [
            // Everyone may read it.
            format!("D:P(A;;FA;;;{user})(A;;FR;;;WD)"),
            // The system and the Administrators group may change it, as in a user profile.
            format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user})"),
            // A rule that only takes write access away from an account.
            format!("D:P(D;;0x6;;;AN)(A;;FA;;;{user})"),
        ] {
            set_access_list(&fixture.pi, &sddl);
            assert!(read_store(&fixture.pi).unwrap().is_some(), "{sddl}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn only_this_processs_owners_and_the_system_may_change_a_store() {
        let sids = |sids: &[&str]| sids.iter().map(|sid| sid.to_string()).collect::<Vec<_>>();
        let verdict = |owner: &str, writers: &[&str], own: &[&str]| {
            windows_store_is_private(owner, &sids(writers), &sids(own))
                .map_err(|error| error.to_string())
        };
        let (user, other) = ("S-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1002");
        let (system, administrators) = ("S-1-5-18", "S-1-5-32-544");
        let differs = Err("trust store owner differs from current user".to_owned());

        // A store in the user profile, read without elevation.
        assert_eq!(
            verdict(user, &[system, administrators, user], &[user]),
            Ok(())
        );
        // A store that an elevated process wrote, read by an elevated process.
        assert_eq!(
            verdict(administrators, &[user, "S-1-3-4"], &[user, administrators]),
            Ok(())
        );
        // The same store read without elevation: the group is not this process's owner.
        assert_eq!(verdict(administrators, &[user], &[user]), differs);
        assert_eq!(verdict(other, &[], &[user]), differs);
        assert_eq!(
            verdict(user, &[user, other], &[user]),
            Err(format!(
                "trust store can be changed by another account ({other})"
            ))
        );
        // Being the default owner of an elevated process does not make the Users group one.
        assert_eq!(
            verdict(administrators, &["S-1-5-32-545"], &[user, administrators]),
            Err("trust store can be changed by another account (S-1-5-32-545)".to_owned())
        );
    }
    #[test]
    fn the_user_home_is_home_even_empty_then_userprofile() {
        let env = Environment {
            home: Some("".into()),
            userprofile: Some("profile".into()),
            ..Default::default()
        };
        assert_eq!(user_home(&env).unwrap(), OsStr::new(""));
        let env = Environment {
            userprofile: Some("profile".into()),
            codex_home: Some("codex".into()),
            ..Default::default()
        };
        assert_eq!(user_home(&env).unwrap(), OsStr::new("profile"));
        // A provider override is no user home.
        let env = Environment {
            codex_home: Some("codex".into()),
            claude_config_dir: Some("claude".into()),
            pi_coding_agent_dir: Some("pi".into()),
            ..Default::default()
        };
        assert_eq!(
            user_home(&env).unwrap_err().to_string(),
            "no user home for workspace trust lookup"
        );
    }
    #[cfg(unix)]
    #[test]
    fn read_store_preserves_write_only_restriction_and_refuses_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store");
        crate::native::session::primitives::write_private(&path, b"store contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_store(&path).unwrap(), Some("store contents".into()));
        for mode in [0o664, 0o606] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_store(&path).unwrap_err().to_string(),
                "trust store is not owned and writable only by the current user"
            );
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = temp.path().join("store-link");
        symlink(&path, &alias).unwrap();
        assert_eq!(
            read_store(&alias).unwrap_err().to_string(),
            "trust store is not a regular file"
        );
    }
}
