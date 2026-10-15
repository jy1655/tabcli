use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::SYNCHRONIZE,
    System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW, WaitForSingleObject,
    },
};

use super::{WindowsProcessIdentity, WindowsProcessIdentityCheck};

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

pub(super) fn open_verified_control_process(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        if !agent_bridge::process_is_alive(pid) {
            bail!("console process is no longer available")
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to retain Windows console process {pid}"));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => bail!("console process is no longer available"),
        WAIT_TIMEOUT => {}
        WAIT_FAILED => {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect retained Windows console process");
        }
        other => bail!("unexpected retained Windows console wait result {other}"),
    }
    let live = query_process_identity_from_handle(handle.as_raw_handle())?;
    verify_identity_values(&live, identity.creation_time, &identity.executable_path)?;
    Ok(handle)
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
    verify_identity_values(&live, creation_time, executable_path)
}

// Compares the live process behind `pid` with a recorded identity and keeps the two
// negative answers apart: an `Err` is a process that could not be inspected and proves
// nothing, while `Mismatch` is a confirmed observation that the pid now belongs to another
// process.
pub(in crate::native::terminal) fn check_process_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<WindowsProcessIdentityCheck> {
    let live = query_process_identity(pid)?;
    Ok(check_identity_values(
        &live,
        identity.creation_time,
        &identity.executable_path,
    ))
}

fn verify_identity_values(
    live: &WindowsProcessIdentity,
    creation_time: u64,
    executable_path: &str,
) -> Result<()> {
    match check_identity_values(live, creation_time, executable_path) {
        WindowsProcessIdentityCheck::Matches => Ok(()),
        WindowsProcessIdentityCheck::Mismatch(reason) => bail!("{reason}"),
    }
}

fn check_identity_values(
    live: &WindowsProcessIdentity,
    creation_time: u64,
    executable_path: &str,
) -> WindowsProcessIdentityCheck {
    if live.creation_time != creation_time {
        return WindowsProcessIdentityCheck::Mismatch("Windows console process id was reused");
    }
    if !live.executable_path.eq_ignore_ascii_case(executable_path) {
        return WindowsProcessIdentityCheck::Mismatch(
            "Windows console process executable identity changed",
        );
    }
    WindowsProcessIdentityCheck::Matches
}

/// A comparison used only by owner attestation; record and surface equality stay exact.
#[derive(Debug)]
pub(in crate::native) struct WindowsIncarnation(pub WindowsProcessIdentity);

impl PartialEq for WindowsIncarnation {
    fn eq(&self, other: &Self) -> bool {
        same_incarnation(&self.0, &other.0)
    }
}

fn same_incarnation(recorded: &WindowsProcessIdentity, live: &WindowsProcessIdentity) -> bool {
    recorded.creation_time == live.creation_time
        && recorded
            .executable_path
            .eq_ignore_ascii_case(&live.executable_path)
}

pub(in crate::native) fn observe_process_identity(
    pid: u32,
) -> Result<Option<WindowsProcessIdentity>> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        // Win32 reports ERROR_INVALID_PARAMETER for a nonexistent PID. Access denial
        // and every other open failure prove no absence and remain inspection errors.
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("failed to open Windows process {pid}"));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    observe_process_identity_from_handle(handle.as_raw_handle())
}

fn observe_process_identity_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<Option<WindowsProcessIdentity>> {
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(None),
        WAIT_TIMEOUT => query_process_identity_from_handle(handle).map(Some),
        WAIT_FAILED => {
            Err(std::io::Error::last_os_error()).context("failed to observe Windows process exit")
        }
        other => bail!("unexpected Windows process wait result {other}"),
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    #[test]
    fn retained_exited_process_is_absent_and_invalid_handle_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit", "0"])
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            observe_process_identity_from_handle(child.as_raw_handle())
                .unwrap()
                .is_none()
        );
        drop(child);
        assert!(observe_process_identity(pid).unwrap().is_none());
        assert!(observe_process_identity_from_handle(std::ptr::null_mut()).is_err());
        assert!(observe_process_identity(u32::MAX).unwrap().is_none());
        assert!(
            observe_process_identity(std::process::id())
                .unwrap()
                .is_some()
        );
    }
}
