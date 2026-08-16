use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
            SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    },
};

pub(in crate::native::terminal) fn set_private_permissions(
    path: &Path,
    directory: bool,
) -> Result<()> {
    let system_directory = system_directory()?;
    let whoami = system_directory.join("whoami.exe");
    let whoami_output = Command::new(&whoami)
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .with_context(|| {
            format!(
                "failed to query the current Windows identity via {}",
                whoami.display()
            )
        })?;
    if !whoami_output.status.success() {
        bail!("failed to query the current Windows identity");
    }
    let identity_output = String::from_utf8_lossy(&whoami_output.stdout);
    let sid = identity_output
        .split([',', '"', '\r', '\n'])
        .map(str::trim)
        .find(|value| {
            value.starts_with("S-1-")
                && value.chars().all(|c| c == '-' || c.is_ascii_alphanumeric())
        })
        .context("whoami did not return a Windows user SID")?;
    apply_private_dacl(path, &private_sddl(sid, directory))
}

pub(super) fn private_sddl(sid: &str, directory: bool) -> String {
    format!("D:P(A;{};FA;;;{sid})", if directory { "OICI" } else { "" })
}

fn apply_private_dacl(path: &Path, sddl: &str) -> Result<()> {
    let descriptor_text = OsStr::new(sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("failed to build a private Windows security descriptor");
    }

    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let extracted =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    if extracted == 0 || present == 0 || dacl.is_null() {
        let error = if extracted == 0 {
            anyhow::Error::new(std::io::Error::last_os_error())
        } else {
            anyhow::anyhow!("private Windows security descriptor contains no DACL")
        };
        unsafe { LocalFree(descriptor) };
        return Err(error).context("failed to extract a private Windows DACL");
    }

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(descriptor) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .context("failed to atomically apply a private Windows DACL");
    }
    Ok(())
}

fn system_directory() -> Result<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut path = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) };
    if length == 0 || length as usize >= path.len() {
        return Err(std::io::Error::last_os_error()).context("failed to resolve Windows System32");
    }
    path.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf16(&path)?))
}
