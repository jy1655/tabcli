use super::*;
use windows_sys::Win32::Security::{
    Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW},
    OWNER_SECURITY_INFORMATION,
};

fn whoami_sid() -> String {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut path = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) };
    assert!(length > 0 && (length as usize) < path.len());
    path.truncate(length as usize);
    let whoami = std::path::PathBuf::from(String::from_utf16(&path).unwrap()).join("whoami.exe");
    let output = crate::native::process_env::helper_command(&whoami)
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .split([',', '"', '\r', '\n'])
        .map(str::trim)
        .find(|value| value.starts_with("S-1-"))
        .unwrap()
        .to_owned()
}

fn descriptor(path: &Path, information: u32) -> String {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            information,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, 0);
    descriptor_text(descriptor, information)
}

fn canonical_dacl(sddl: &str) -> String {
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        },
        0
    );
    descriptor_text(descriptor, DACL_SECURITY_INFORMATION)
}

// Both callers transfer the LocalAlloc descriptor, including on conversion failure.
fn descriptor_text(descriptor: *mut std::ffi::c_void, information: u32) -> String {
    let mut text = std::ptr::null_mut();
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            information,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(descriptor) };
    assert_ne!(converted, 0);
    let result = unsafe {
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, length))
    };
    unsafe { LocalFree(text.cast()) };
    result.unwrap()
}

#[test]
fn token_user_matches_the_former_whoami_identity() {
    assert_eq!(process_user_sid().unwrap(), whoami_sid());
}

#[test]
fn private_permissions_replace_broad_access_preserve_owner_and_reapply() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("record");
    std::fs::write(&file, b"fixture").unwrap();
    let sid = whoami_sid();
    for (path, directory, expected) in [
        (root.path(), true, format!("D:P(A;OICI;FA;;;{sid})")),
        (file.as_path(), false, format!("D:P(A;;FA;;;{sid})")),
    ] {
        // SDDL uses aliases for identities such as LocalSystem or built-in Administrator.
        let expected = canonical_dacl(&expected);
        let owner = descriptor(path, OWNER_SECURITY_INFORMATION);
        for _ in 0..2 {
            // Reintroduce an unrelated writer: an earlier successful call must never
            // cache permission authority or skip tightening an existing record.
            apply_private_dacl(path, "D:P(A;;FA;;;WD)").unwrap();
            set_private_permissions(path, directory).unwrap();
            let actual = descriptor(path, DACL_SECURITY_INFORMATION);
            // Windows can retain SE_DACL_AUTO_INHERITED from the original descriptor.
            // It grants nothing: the protected flag and the sole ACE must still match.
            assert!(
                actual == expected || actual == expected.replacen("D:P(", "D:PAI(", 1),
                "unexpected DACL: {actual}"
            );
            assert_eq!(descriptor(path, OWNER_SECURITY_INFORMATION), owner);
        }
    }
    assert_eq!(std::fs::read(file).unwrap(), b"fixture");
}

#[test]
fn private_permissions_report_an_os_failure() {
    let root = tempfile::tempdir().unwrap();
    let error = set_private_permissions(&root.path().join("absent"), false).unwrap_err();
    assert!(format!("{error:#}").contains("failed to atomically apply a private Windows DACL"));
}
