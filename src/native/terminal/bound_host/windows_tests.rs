//! The Windows admission of a bound host's session directory, read from a handle on the
//! directory itself: a private directory is admitted; one that another account can change,
//! a junction to a private directory and a file are refused.
use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SPEC: BoundHostSpec = BoundHostSpec { name: "Herdr" };

fn private_session(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    crate::native::session::primitives::set_private_directory_permissions(&directory).unwrap();
    directory
}

pub(super) fn icacls(path: &Path, grant: &str) {
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    let status = Command::new(PathBuf::from(system).join(r"System32\icacls.exe"))
        .arg(path)
        .args(["/grant", grant])
        .status()
        .unwrap();
    assert!(status.success(), "icacls /grant {grant}");
}

// Replaces the access list of a fixture with the one that `sddl` describes.
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

fn refusal(directory: &Path) -> String {
    format!(
        "{:#}",
        run_bound_host(directory, &SPEC, |id| Ok(id.to_owned())).unwrap_err()
    )
}

#[test]
fn a_private_session_directory_is_admitted_from_its_handle() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-admitted");
    assert_eq!(
        run_bound_host(&directory, &SPEC, |id| Ok(id.to_owned())).unwrap(),
        "session-admitted"
    );
}

#[test]
fn native_herdr_host_dispatch_reaches_the_launch_receipt_check() {
    use super::super::host;
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-herdr-host");
    // The launch log the launcher creates when the launch begins; the host writes its refusal
    // only into it.
    crate::native::session::primitives::write_private(&directory.join(launch::LOG), b"").unwrap();
    let command = host::parse(
        host::HERDR_HOST_COMMAND,
        &[
            directory.to_str().unwrap().to_owned(),
            host::LAUNCH_COMMAND_FLAG.to_owned(),
            "must not run".to_owned(),
        ],
    )
    .unwrap();
    let error = host::run(command).unwrap_err();
    assert_eq!(error.to_string(), "missing Herdr launch receipt");
    let log = fs::read_to_string(directory.join(launch::LOG)).unwrap();
    assert!(log.ends_with(" herdr_host_refused: missing Herdr launch receipt\n"));
}

// Everyone (S-1-1-0) gains a right that changes the directory: writing an entry, or only
// deleting entries, which the trust store's own rule does not count for a file.
#[test]
fn a_session_directory_that_another_account_can_change_is_refused() {
    for grant in ["*S-1-1-0:(W)", "*S-1-1-0:(DC)", "*S-1-1-0:(D)"] {
        let root = tempfile::tempdir().unwrap();
        let directory = private_session(root.path(), "session-hostile");
        icacls(&directory, grant);
        let text = refusal(&directory);
        assert!(
            text.starts_with("Herdr launch directory is not private to the current user"),
            "{grant}: {text}"
        );
        assert!(
            text.contains("session directory can be changed by another account (S-1-1-0)"),
            "{grant}: {text}"
        );
    }
}

#[test]
fn a_junction_or_a_file_is_never_a_session_directory() {
    let root = tempfile::tempdir().unwrap();
    let target = private_session(root.path(), "session-target");
    let junction = root.path().join("session-junction");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J");
    let text = refusal(&junction);
    assert!(
        text.contains("refusing non-directory Herdr launch path"),
        "{text}"
    );
    let file = root.path().join("session-file");
    fs::write(&file, b"not a directory").unwrap();
    let text = refusal(&file);
    assert!(
        text.contains("refusing non-directory Herdr launch path"),
        "{text}"
    );
}

// Windows runs one bound host, Herdr's, which runs as its core: the launcher judges no kind
// itself, and the Herdr adapter judges a directory that another account can change for the
// core's account. Without that account only the path is judged: a real directory, never a
// junction; who may change it is left to the host.
#[test]
fn the_herdr_check_refuses_a_directory_the_launch_host_would_refuse() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-launcher");
    let own = HostAccount::current().unwrap();
    refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&own)).unwrap();
    icacls(&directory, "*S-1-1-0:(W)");
    let refusal =
        refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&own)).unwrap_err();
    let text = format!("{refusal:#}");
    assert_eq!(
        text,
        format!(
            "the Herdr launch host would refuse the session directory {}: Herdr launch directory is not private to the current user: session directory can be changed by another account (S-1-1-0)",
            directory.display()
        )
    );
    refuse_before_core_launch(&directory, TerminalKind::Herdr, None).unwrap();
    for kind in [
        TerminalKind::Herdr,
        TerminalKind::WindowsConsole,
        TerminalKind::Tmux,
        TerminalKind::Iterm2,
    ] {
        refuse_before_launch(&directory, kind).unwrap();
    }
    let target = private_session(root.path(), "session-target");
    let junction = root.path().join("session-junction");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J");
    let text = format!(
        "{:#}",
        refuse_before_core_launch(&junction, TerminalKind::Herdr, None).unwrap_err()
    );
    assert!(
        text.contains("refusing non-directory Herdr launch path"),
        "{text}"
    );
}

// The launcher refuses only what it observed: a read that TAB's own token is denied says
// nothing about what the core's launch host, which reads the directory as the core's account,
// will find. Here an owner-rights rule without `READ_CONTROL` replaces the owner's implicit
// right to read the access list, so TAB cannot open the directory to read its owner and access
// list for the core's account: the check leaves it to the host, and the host's own admission
// (run here as this process) refuses it.
#[test]
fn a_session_directory_whose_access_list_tab_cannot_read_is_left_to_the_launch_host() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL};
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-unreadable");
    // Owner rights: every right of `FILE_ALL_ACCESS` (0x1F01FF) but `READ_CONTROL` (0x20000).
    set_access_list(&directory, "D:P(A;;0x1D01FF;;;OW)");
    let denied = fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&directory)
        .unwrap_err();
    assert_eq!(
        denied.kind(),
        std::io::ErrorKind::PermissionDenied,
        "{denied}"
    );
    let own = HostAccount::current().unwrap();
    refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&own)).unwrap();
    refuse_before_core_launch(&directory, TerminalKind::Herdr, None).unwrap();
    let host = refusal(&directory);
    assert!(
        host.starts_with("cannot open Herdr launch directory"),
        "{host}"
    );
}

// The case observed on 2026-11-26: an elevated TAB creates the session directory, so its owner
// is the default owner of TAB's token (the Administrators group), while a core that is not
// elevated accepts only its user as owner. The Windows CI runner is elevated, so there this
// is that case; a TAB that is not elevated owns the directory as its user, which the core's
// launch host admits.
#[test]
fn an_elevated_tab_is_refused_for_a_core_that_is_not_elevated() {
    let root = tempfile::tempdir().unwrap();
    let directory = private_session(root.path(), "session-elevation");
    let own = HostAccount::current().unwrap();
    refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&own)).unwrap();
    let user = own.windows.owners[0].clone();
    let core = HostAccount::of_windows(crate::native::consent::WindowsAccount {
        owners: vec![user],
        elevated: Some(false),
    });
    let result = refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&core));
    if own.windows.owners.len() == 1 {
        result.unwrap();
        return;
    }
    let text = format!("{:#}", result.unwrap_err());
    assert!(
        text.contains("session directory owner differs from current user"),
        "{text}"
    );
    if own.windows.elevated == Some(true) {
        assert!(
            text.contains("TAB is running elevated and the Herdr Core is not"),
            "{text}"
        );
    }
    // A directory TAB's own account would refuse as well is refused for its reason alone: no
    // advice about the accounts follows the host's reason. (The fixture's own name contains
    // "elevation", so only the advice clauses are looked for.)
    icacls(&directory, "*S-1-1-0:(W)");
    let text = format!(
        "{:#}",
        refuse_before_core_launch(&directory, TerminalKind::Herdr, Some(&core)).unwrap_err()
    );
    assert!(
        text.ends_with("session directory owner differs from current user"),
        "{text}"
    );
    assert!(!text.contains("TAB is running elevated"), "{text}");
    assert!(
        !text.contains("give their files different owners"),
        "{text}"
    );
}

// A session directory whose path cannot be sent to the launch host as text exactly is refused
// before any Surface exists.
#[test]
fn a_session_directory_that_cannot_be_sent_as_text_is_refused_before_any_surface() {
    use std::os::windows::ffi::OsStringExt;
    let mut wide: Vec<u16> = r"C:\state-".encode_utf16().collect();
    wide.push(0xD800);
    wide.extend(r"\session-text".encode_utf16());
    let directory = PathBuf::from(std::ffi::OsString::from_wide(&wide));
    let text = format!(
        "{:#}",
        refuse_before_launch(&directory, TerminalKind::Herdr).unwrap_err()
    );
    assert!(
        text.ends_with(
            "Herdr launch directory is not Unicode text, which is how the launch host is given it"
        ),
        "{text}"
    );
}
