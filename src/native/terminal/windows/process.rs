use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::SYNCHRONIZE,
    System::Threading::{
        GetProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    },
};

use super::WindowsProcessIdentity;

pub(super) fn verify_control_process_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<bool> {
    anyhow::ensure!(pid != 0, "invalid Windows console pid");
    let Some(live) = observe_process_identity(pid)? else {
        return Ok(false);
    };
    verify_identity_values(&live, identity.creation_time, &identity.executable_path)?;
    Ok(true)
}

pub(super) fn open_verified_control_process(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<OwnedHandle> {
    anyhow::ensure!(pid != 0, "invalid Windows console pid");
    let (handle, live) =
        open_observed_process(pid, 0)?.context(crate::native::console_control::MISSING_PROCESS)?;
    verify_identity_values(&live, identity.creation_time, &identity.executable_path)?;
    Ok(handle)
}

/// A process held open for limited query, so that every read of its token is of the process
/// first opened: Windows reuses no PID while a handle to its process is open. Its token is
/// read through the security and trust-store modules, which open no process.
pub(in crate::native) struct QueriedProcess(OwnedHandle);

impl QueriedProcess {
    pub(in crate::native) fn open(pid: u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open Windows process {pid}"));
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    /// The user SID in its primary token: whose process it is. A process this one may not
    /// inspect is an error, never another user.
    pub(in crate::native) fn user_sid(&self) -> Result<String> {
        super::security::process_handle_user_sid(&self.0)
    }

    /// Its account (`consent::windows_process_account`): the owners its new files can have
    /// and whether it runs elevated.
    pub(in crate::native) fn account(&self) -> Result<crate::native::consent::WindowsAccount> {
        crate::native::consent::windows_process_account(&self.0)
    }
}

pub(in crate::native) fn query_process_identity(pid: u32) -> Result<WindowsProcessIdentity> {
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

fn verify_identity_values(
    live: &WindowsProcessIdentity,
    creation_time: u64,
    executable_path: &str,
) -> Result<()> {
    check_identity_values(live, creation_time, executable_path).map_err(anyhow::Error::msg)
}

pub(super) fn check_identity_values(
    live: &WindowsProcessIdentity,
    creation_time: u64,
    executable_path: &str,
) -> std::result::Result<(), &'static str> {
    if live.creation_time != creation_time {
        return Err("Windows console process id was reused");
    }
    if !live.executable_path.eq_ignore_ascii_case(executable_path) {
        return Err("Windows console process executable identity changed");
    }
    Ok(())
}

/// Opens with PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE to observe exit first.
/// A process granting only QUERY access yields an inspection error, not absence.
pub(in crate::native) fn observe_process_identity(
    pid: u32,
) -> Result<Option<WindowsProcessIdentity>> {
    Ok(open_observed_process(pid, 0)?.map(|(_, identity)| identity))
}

/// Keep the kernel object pinned from the first observation through any subsequent action.
pub(super) fn open_observed_process(
    pid: u32,
    additional_access: u32,
) -> Result<Option<(OwnedHandle, WindowsProcessIdentity)>> {
    let handle = unsafe {
        OpenProcess(
            additional_access | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            pid,
        )
    };
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
    Ok(
        observe_process_identity_from_handle(handle.as_raw_handle())?
            .map(|identity| (handle, identity)),
    )
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

pub(in crate::native::terminal) fn pin_teardown_process(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<Option<OwnedHandle>> {
    anyhow::ensure!(
        identity.creation_time != 0 && !identity.executable_path.is_empty(),
        "incomplete recorded Windows process identity"
    );
    let Some((handle, live)) = open_observed_process(pid, PROCESS_TERMINATE)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        unsafe { GetProcessId(handle.as_raw_handle()) } == pid,
        "Windows process handle has a different PID"
    );
    verify_identity_values(&live, identity.creation_time, &identity.executable_path)?;
    Ok(Some(handle))
}

impl crate::native::terminal::close_authority::windows_herdr::PinnedProcess for OwnedHandle {
    fn exited(&self) -> Result<bool> {
        match unsafe { WaitForSingleObject(self.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(std::io::Error::last_os_error())
                .context("failed to observe Windows process exit"),
            other => bail!("unexpected Windows process wait result {other}"),
        }
    }

    fn stop(&self, deadline: std::time::Instant) -> Result<bool> {
        use crate::native::session::close::budget::close_remaining;
        if self.exited()? {
            return Ok(false);
        }
        close_remaining(deadline)?;
        if unsafe { TerminateProcess(self.as_raw_handle(), 1) } == 0 {
            let error = std::io::Error::last_os_error();
            if self.exited()? {
                return Ok(false);
            }
            return Err(error).context("failed to terminate Windows process");
        }
        let remaining = close_remaining(deadline)?;
        let millis = remaining.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        match unsafe { WaitForSingleObject(self.as_raw_handle(), millis) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => {
                bail!("Windows process exit was not confirmed within the close deadline")
            }
            WAIT_FAILED => Err(std::io::Error::last_os_error())
                .context("failed to wait for Windows process exit"),
            other => bail!("unexpected Windows process wait result {other}"),
        }
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    #[test]
    fn observing_pid_zero_reports_no_process() {
        assert!(observe_process_identity(0).unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn retained_exited_process_is_unqueryable_and_observed_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Keep cmd alive long enough to capture its identity before ending it.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 3 127.0.0.1 >NUL"])
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let pid = child.id();
        let recorded = query_process_identity(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        recorded.unwrap();
        // The Child still retains the process object. Windows refuses the image-path query
        // of an exited process (observed on the GitHub runner: "A device attached to the
        // system is not functioning", os error 31), so the former typed check reached its
        // liveness fallback and reported the process gone; the observer reports the same
        // absence from the signalled handle without consulting the liveness bool.
        assert!(query_process_identity(pid).is_err());
        assert!(observe_process_identity(pid).unwrap().is_none());
        drop(child);
        // With the last handle closed, ERROR_INVALID_PARAMETER also proves absence.
        assert!(observe_process_identity(pid).unwrap().is_none());
    }

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
