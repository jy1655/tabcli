use std::{
    ffi::OsStr,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub(in crate::native) fn set_private_permissions(path: &Path, directory: bool) -> Result<()> {
    let sid = process_user_sid()?;
    apply_private_dacl(path, &private_sddl(&sid, directory))
}

// CreateProcess (and thus the former whoami subprocess) uses the process's primary
// token even when the calling thread impersonates. Read that same TokenUser, not the
// thread token or TokenOwner (which can be Administrators on an elevated runner).
// Query on every call: neither a cached identity nor cached permissions are authority.
fn process_user_sid() -> Result<String> {
    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open the Windows process token");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut needed = 0;
    let queried = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    let error = std::io::Error::last_os_error();
    if queried != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(error).context("failed to size the Windows token user");
    }
    if (needed as usize) < std::mem::size_of::<TOKEN_USER>() {
        bail!("Windows token user is shorter than TOKEN_USER");
    }
    // TOKEN_USER contains a pointer, so a byte vector would not promise its alignment.
    let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("failed to read the Windows token user");
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to format the Windows user SID");
    }
    let sid = unsafe {
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, length))
    };
    unsafe { LocalFree(text.cast()) };
    sid.context("Windows user SID is not valid UTF-16")
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

#[cfg(test)]
mod tests;
