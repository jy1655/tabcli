use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
};

use super::WindowsProcessIdentity;

pub(super) fn verify_control_process_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<()> {
    match verify_process_identity(pid, identity.creation_time, &identity.executable_path) {
        Ok(()) => Ok(()),
        Err(_) if !agent_bridge::process_is_alive(pid) => {
            bail!("console process is no longer available")
        }
        Err(error) => Err(error),
    }
}

pub(in crate::native::terminal) fn query_process_identity(
    pid: u32,
) -> Result<WindowsProcessIdentity> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open Windows process {pid}"));
    }
    let result = query_process_identity_from_handle(handle);
    unsafe { CloseHandle(handle) };
    result
}

pub(super) fn query_process_identity_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<WindowsProcessIdentity> {
    let mut creation = windows_sys::Win32::Foundation::FILETIME::default();
    let mut exit = windows_sys::Win32::Foundation::FILETIME::default();
    let mut kernel = windows_sys::Win32::Foundation::FILETIME::default();
    let mut user = windows_sys::Win32::Foundation::FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to query process creation time");
    }
    let mut path = vec![0u16; 32768];
    let mut path_len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut path_len) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to query process executable path");
    }
    path.truncate(path_len as usize);
    Ok(WindowsProcessIdentity {
        creation_time: (u64::from(creation.dwHighDateTime) << 32)
            | u64::from(creation.dwLowDateTime),
        executable_path: String::from_utf16(&path)
            .context("process executable path is not valid UTF-16")?,
    })
}

pub(in crate::native::terminal) fn verify_process_identity(
    pid: u32,
    creation_time: u64,
    executable_path: &str,
) -> Result<()> {
    let live = query_process_identity(pid)?;
    if live.creation_time != creation_time {
        bail!("Windows console process id was reused");
    }
    if !live.executable_path.eq_ignore_ascii_case(executable_path) {
        bail!("Windows console process executable identity changed");
    }
    Ok(())
}
