use super::*;
use std::os::windows::{
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::process::Command;
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT, SetSecurityInfo,
    },
    Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL, WRITE_DAC, WRITE_OWNER},
};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError},
    Security::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const CHILD: &str = "TABCLI_STATE_ROOT_TEST_CHILD";

fn icacls(root: &Path, args: &[&str]) {
    let tool = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\icacls.exe");
    let output = Command::new(tool).arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}

// Keep a handle with WRITE_DAC across the denial fixtures so cleanup needs
// neither the child's privileges nor a successfully repaired directory.
struct SavedAccess {
    root: fs::File,
    descriptor: *mut std::ffi::c_void,
    dacl: *mut ACL,
}
impl SavedAccess {
    fn capture(path: &Path, rights: u32) -> Self {
        let root = fs::OpenOptions::new()
            .access_mode(rights | READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        let mut descriptor = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    root.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        Self {
            root,
            descriptor,
            dacl,
        }
    }
    fn owner(&self) -> String {
        let mut sid = std::ptr::null_mut();
        let mut defaulted = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorOwner(self.descriptor, &mut sid, &mut defaulted) },
            0
        );
        let mut text = std::ptr::null_mut();
        assert_ne!(
            unsafe { Authorization::ConvertSidToStringSidW(sid, &mut text) },
            0
        );
        let mut length = 0;
        unsafe {
            while *text.add(length) != 0 {
                length += 1;
            }
        }
        let result =
            String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap();
        unsafe {
            LocalFree(text.cast());
        }
        result
    }
    fn as_text(&self) -> String {
        let mut text = std::ptr::null_mut();
        let mut length = 0;
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    self.descriptor,
                    SDDL_REVISION_1,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut text,
                    &mut length,
                )
            },
            0
        );
        let value =
            String::from_utf16(unsafe { std::slice::from_raw_parts(text, length as usize) })
                .unwrap();
        unsafe {
            LocalFree(text.cast());
        }
        value.trim_end_matches('\0').to_owned()
    }
    fn apply_dacl_to(&self, file: &fs::File) {
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    self.dacl,
                    std::ptr::null_mut(),
                )
            },
            ERROR_SUCCESS,
            "SetSecurityInfo: apply saved DACL"
        );
    }
}
impl Drop for SavedAccess {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

fn child_token(user_default: bool) {
    let mut raw = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_PRIVILEGES,
                &mut raw,
            )
        },
        0
    );
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let token = token.as_raw_handle();
    for name in [SE_TAKE_OWNERSHIP_NAME, SE_RESTORE_NAME, SE_BACKUP_NAME] {
        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            ..Default::default()
        };
        assert_ne!(
            unsafe {
                LookupPrivilegeValueW(std::ptr::null(), name, &mut privileges.Privileges[0].Luid)
            },
            0
        );
        assert_ne!(
            unsafe {
                AdjustTokenPrivileges(
                    token,
                    0,
                    &privileges,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert!(matches!(
            unsafe { GetLastError() },
            ERROR_SUCCESS | ERROR_NOT_ALL_ASSIGNED
        ));
    }
    if user_default {
        // Exercise the primary token's real owner membership even on elevated CI.
        let mut needed = 0;
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut user = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    user.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            },
            0
        );
        let owner = TOKEN_OWNER {
            Owner: unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid },
        };
        assert_ne!(
            unsafe {
                SetTokenInformation(
                    token,
                    TokenOwner,
                    (&raw const owner).cast(),
                    size_of::<TOKEN_OWNER>() as u32,
                )
            },
            0
        );
    }
}

fn probe_right(root: &Path, right: u32) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .access_mode(right)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root)
}

fn assert_denied(root: &Path, right: u32, stage: &str) {
    let access = probe_right(root, right);
    eprintln!("{stage}: open for {right:#x} returned {access:?}");
    assert_eq!(
        access.as_ref().err().and_then(std::io::Error::raw_os_error),
        Some(ERROR_ACCESS_DENIED as i32),
        "{stage}: open for {right:#x} returned {access:?}; {}",
        SavedAccess::capture(root, 0).as_text()
    );
}

#[test]
fn windows_state_root_owners_and_dacl() {
    if let Some(case) = std::env::var_os(CHILD) {
        let case = case.to_str().unwrap();
        let root = PathBuf::from(std::env::var_os("TABCLI_ROOT_FIXTURE").unwrap());
        child_token(case != "accepted");
        let account = crate::native::consent::windows_current_account().unwrap();
        let administrators = "S-1-5-32-544";
        if case != "accepted" {
            assert!(!account.owners.iter().any(|sid| sid == administrators));
        }
        let observed = SavedAccess::capture(&root, 0);
        let expected_owner = match case {
            "foreign" => "S-1-5-18",
            "retry" => &account.owners[0],
            "accepted" => account.owners.last().unwrap().as_str(),
            _ => administrators,
        };
        assert_eq!(observed.owner(), expected_owner);
        let before = observed.as_text();
        let doctor = crate::native::doctor::state_root_check();
        assert_eq!(SavedAccess::capture(&root, 0).as_text(), before);
        if matches!(case, "repair" | "partial") {
            probe_right(&root, WRITE_OWNER).unwrap_or_else(|error| {
                panic!("{case}: fixture must grant WRITE_OWNER: {error}; {before}")
            });
        }
        if case == "denied" {
            assert_denied(&root, WRITE_OWNER, "denied before preparation");
        }
        if case == "unmarked" {
            assert!(unsafe { (*observed.dacl).AceCount } > 1);
        }
        let create = || create_directory(&root, &[]);
        let result = if case == "partial" {
            assert_denied(&root, WRITE_DAC, "partial before preparation");
            let path = root.clone();
            let user = account.owners[0].clone();
            crate::native::session::primitives::with_step_at(
                "after setting the state root owner",
                move || {
                    let transferred = SavedAccess::capture(&path, 0);
                    assert_eq!(transferred.owner(), user, "partial after owner transfer");
                    eprintln!("partial after owner transfer: {}", transferred.as_text());
                    // Taking ownership can disable the OWNER RIGHTS ACE. Reapply
                    // the original DACL for the new owner while we can still write it.
                    match probe_right(&path, READ_CONTROL | WRITE_DAC) {
                        Ok(writer) => observed.apply_dacl_to(&writer),
                        Err(error) => assert_eq!(
                            error.raw_os_error(),
                            Some(ERROR_ACCESS_DENIED as i32),
                            "partial after owner transfer: {error}"
                        ),
                    }
                    assert_denied(&path, WRITE_DAC, "partial before DACL repair");
                },
                create,
            )
        } else {
            create()
        };
        match &result {
            Ok(session) => eprintln!("{case}: create_directory returned Ok({session:?}); {before}"),
            Err(error) => eprintln!("{case}: create_directory returned Err({error:#}); {before}"),
        }
        match case {
            "accepted" | "repair" | "retry" => {
                result.unwrap();
            }
            "denied" | "partial" => {
                let error = result.unwrap_err();
                let expected = if case == "denied" {
                    "NtOpenFile: reopen directory to set owner (access 0x80000)"
                } else {
                    "NtOpenFile: reopen directory to set DACL (access 0x60080)"
                };
                assert!(
                    error.chain().any(|e| e.to_string() == expected),
                    "{error:#}"
                );
                assert_eq!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .and_then(|e| e.raw_os_error()),
                    Some(5),
                    "{error:#}"
                );
            }
            "foreign" => {
                assert!(result.is_err(), "{case} unexpectedly created a Session")
            }
            "unmarked" => {
                let expected = format!(
                    "state root {}: owner SID {administrators}, expected {}; the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory); set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB",
                    root.display(),
                    account.owners.join(" or ")
                );
                assert_eq!(doctor.detail, expected);
                assert_eq!(
                    doctor.next_action,
                    "set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB"
                );
                assert_eq!(format!("{:#}", result.unwrap_err()), expected);
                assert_private_dacl(&SavedAccess::capture(&root, 0));
                let error =
                    crate::native::session::Store::create_settings_root(&root, &[]).unwrap_err();
                assert_eq!(format!("{error:#}"), expected);
                assert!(fs::read_dir(&root).unwrap().next().is_none());
            }
            _ => panic!("unknown case"),
        }
        let actual = SavedAccess::capture(&root, 0).owner();
        let expected = match case {
            "repair" | "partial" | "retry" => account.owners[0].as_str(),
            "foreign" => "S-1-5-18",
            "accepted" => expected_owner,
            _ => administrators,
        };
        assert_eq!(actual, expected);
        return;
    }
    let account = crate::native::consent::windows_current_account().unwrap();
    // Setting SYSTEM as a fixture owner needs an elevated setup process.
    if account.elevated != Some(true) {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must provision owner fixtures"
        );
        eprintln!("foreign-owner fixtures require an elevated Windows account");
        return;
    }
    let user = &account.owners[0];
    for case in [
        "accepted", "repair", "foreign", "denied", "partial", "unmarked",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        if case != "unmarked" {
            fs::create_dir(path.join("workspace-consent")).unwrap();
        }
        let original = SavedAccess::capture(path, WRITE_DAC);
        // CI temporary roots can have an explicit Administrators full-control ACE.
        // Reset it to inherited grants before constructing the fixture's explicit DACL.
        icacls(path, &["/reset"]);
        // /grant:r replaces an explicit allow ACE with the same inheritance flags.
        // Give setup and child grants the same scope so full control cannot survive.
        let (scope, rights) = match case {
            "denied" | "foreign" => ("", "(RD,REA,RA,X,RC,S)"),
            "partial" => ("", "(RD,REA,RA,X,RC,S,WO)"),
            _ => ("(OI)(CI)", "F"),
        };
        icacls(path, &["/grant:r", &format!("*{user}:{scope}F")]);
        let owner = match case {
            "foreign" => "S-1-5-18",
            "accepted" => account.owners.last().unwrap().as_str(),
            _ => "S-1-5-32-544",
        };
        icacls(path, &["/setowner", &format!("*{owner}")]);
        icacls(path, &["/inheritance:r"]);
        if case == "partial" {
            icacls(path, &["/grant:r", "*S-1-3-4:(RC)"]);
        }
        icacls(path, &["/grant:r", &format!("*{user}:{scope}{rights}")]);
        if case == "unmarked" {
            icacls(path, &["/grant", "*S-1-1-0:(R)"]);
        }
        let run = |case| {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "windows_state_root_owners_and_dacl",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, case)
                .env("AGENT_BRIDGE_NATIVE_STATE_DIR", path)
                .env("TABCLI_ROOT_FIXTURE", path)
                .output()
                .unwrap();
            original.apply_dacl_to(&original.root);
            assert!(
                output.status.success(),
                "{case}: child exited with {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(case);
        if case == "partial" {
            run("retry");
        }
    }
}

#[test]
fn junction_prepares_the_target_and_preserves_its_owner() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("directory");
    let link = temp.path().join("junction");
    fs::create_dir(&target).unwrap();
    let owner = SavedAccess::capture(&target, 0).owner();
    let out = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    create_state_root(&link, &[]).unwrap();
    assert_eq!(SavedAccess::capture(&target, 0).owner(), owner);
    let saved = SavedAccess::capture(&target, 0);
    assert_private_dacl(&saved);
    assert!(target.join(STATE_ROOT_DURABLE_FILE).is_file());
}

fn assert_private_dacl(saved: &SavedAccess) {
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(saved.descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    assert!(!saved.dacl.is_null());
    assert_eq!(unsafe { (*saved.dacl).AceCount }, 1);
    let mut ace = std::ptr::null_mut();
    assert_ne!(unsafe { GetAce(saved.dacl, 0, &mut ace) }, 0);
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(allowed.Header.AceType, 0);
    assert_eq!(
        allowed.Header.AceFlags,
        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
    );
    assert_eq!(
        allowed.Mask,
        windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS
    );
    let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
    assert_eq!(
        crate::native::consent::windows_sid(sid).unwrap(),
        crate::native::consent::windows_current_account()
            .unwrap()
            .owners[0]
    );
}
