use super::super::ownership::{MacTerminalAppIdentity, NativeProcessIdentity};
use anyhow::{Context, Result, bail};
use std::{fs, path::Path};

#[cfg(target_os = "macos")]
#[repr(C)]
pub(in crate::native) struct MacProcBsdInfo {
    _flags: u32,
    _status: u32,
    _exit_status: u32,
    pid: u32,
    pub(in crate::native) parent_pid: u32,
    _uid: u32,
    _gid: u32,
    _real_uid: u32,
    _real_gid: u32,
    _saved_uid: u32,
    _saved_gid: u32,
    _reserved: u32,
    _command: [libc::c_char; 16],
    _name: [libc::c_char; 32],
    _open_files: u32,
    process_group: u32,
    _job_control_count: u32,
    terminal_tty_device: u32,
    terminal_process_group: u32,
    _nice: i32,
    pub(in crate::native) process_start_seconds: u64,
    pub(in crate::native) process_start_microseconds: u64,
}

// `struct kinfo_proc` of <sys/sysctl.h> on 64-bit macOS, which libc does not define: the
// start time, the PID, the command name and the parent PID at their offsets (0, 8, 40, 243
// and 560 of 648 bytes), the rest as padding.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
pub(in crate::native) struct MacKinfoProc {
    start_seconds: i64,
    start_microseconds: i32,
    _to_pid: [u8; 28],
    pid: i32,
    _to_command: [u8; 199],
    command: [u8; 17],
    _to_parent: [u8; 300],
    pub(in crate::native) parent_pid: i32,
    _rest: [u8; 84],
}

#[cfg(target_os = "macos")]
const _: () = assert!(std::mem::size_of::<MacKinfoProc>() == 648);

#[cfg(target_os = "macos")]
impl MacKinfoProc {
    pub(in crate::native) fn identity(&self) -> Result<MacTerminalAppIdentity> {
        Ok(MacTerminalAppIdentity {
            pid: u32::try_from(self.pid)?,
            start_seconds: u64::try_from(self.start_seconds)?,
            start_microseconds: u64::try_from(self.start_microseconds)?,
        })
    }
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
    pub(in crate::native) fn proc_pidpath(
        pid: libc::c_int,
        buffer: *mut libc::c_void,
        buffer_size: u32,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn current_terminal_tty() -> Result<String> {
    let mut buffer = [0 as libc::c_char; libc::PATH_MAX as usize];
    let error = unsafe { libc::ttyname_r(libc::STDIN_FILENO, buffer.as_mut_ptr(), buffer.len()) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error))
            .context("failed to resolve native-session controlling TTY");
    }
    let tty = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .context("native-session controlling TTY is not valid UTF-8")?;
    Ok(tty.to_owned())
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn terminal_tty_device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect terminal TTY {}", path.display()))?;
    if !metadata.file_type().is_char_device() {
        bail!("terminal TTY is not a character device: {}", path.display())
    }
    Ok(metadata.rdev())
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn live_native_process_identity(pid: u32) -> Result<NativeProcessIdentity> {
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    let pid_value = libc::c_int::try_from(pid).context("native-session PID is out of range")?;
    let buffer_size = libc::c_int::try_from(std::mem::size_of::<MacProcBsdInfo>())
        .context("macOS process-info structure is too large")?;
    let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
    let returned = unsafe {
        proc_pidinfo(
            pid_value,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if returned != buffer_size {
        if returned <= 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to inspect native-session process {pid}"));
        }
        bail!("macOS returned an incomplete identity for native-session process {pid}")
    }
    let info = unsafe { info.assume_init() };
    if info.pid != pid {
        bail!("macOS returned the wrong native-session process identity")
    }
    if info.terminal_tty_device == u32::MAX {
        bail!("native-session process has no controlling TTY")
    }
    Ok(NativeProcessIdentity {
        pid,
        parent_pid: info.parent_pid,
        terminal_tty_device: u64::from(info.terminal_tty_device),
        process_group: info.process_group,
        terminal_process_group: info.terminal_process_group,
        process_start_seconds: info.process_start_seconds,
        process_start_microseconds: info.process_start_microseconds,
    })
}

// The birth of any process, with or without a controlling TTY. `None`: no such process.
#[cfg(target_os = "macos")]
pub(in crate::native) fn macos_process_start(pid: u32) -> Result<Option<(u64, u64)>> {
    Ok(macos_process_info(pid)?
        .map(|info| (info.process_start_seconds, info.process_start_microseconds)))
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn macos_process_info(pid: u32) -> Result<Option<MacProcBsdInfo>> {
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    let pid_value = libc::c_int::try_from(pid).context("PID is out of range")?;
    let buffer_size = libc::c_int::try_from(std::mem::size_of::<MacProcBsdInfo>())
        .context("macOS process-info structure is too large")?;
    let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
    let returned = unsafe {
        proc_pidinfo(
            pid_value,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if returned != buffer_size {
        let error = std::io::Error::last_os_error();
        if returned <= 0 && error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("failed to inspect process {pid}"));
    }
    let info = unsafe { info.assume_init() };
    if info.pid != pid {
        bail!("macOS returned the identity of another process for {pid}")
    }
    Ok(Some(info))
}

// The kernel's process records for one sysctl name, as `ps` reads them. They answer for
// the processes of every user. PROC_PIDTBSDINFO answers only for the caller's own, and
// the shell of a terminal tab is a child of the root-owned /usr/bin/login.
#[cfg(target_os = "macos")]
pub(in crate::native) fn macos_process_records(
    name: &mut [libc::c_int],
) -> Result<Vec<MacKinfoProc>> {
    let record = std::mem::size_of::<MacKinfoProc>();
    let length = libc::c_uint::try_from(name.len())?;
    let mut size = 0;
    let sized = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            length,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if sized != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to size the process table");
    }
    // Room for processes that start between the two calls. A table that grew past it is
    // an error of the second call, never a short list.
    let mut records = vec![unsafe { std::mem::zeroed::<MacKinfoProc>() }; size / record + 64];
    let mut size = records.len() * record;
    let read = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            length,
            records.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read the process table");
    }
    if !size.is_multiple_of(record) {
        bail!("macOS returned an incomplete process record")
    }
    records.truncate(size / record);
    Ok(records)
}

// The executable of a process of any user. `None`: no such process.
#[cfg(target_os = "macos")]
pub(in crate::native) fn macos_process_path(pid: u32) -> Result<Option<String>> {
    let mut path = [0u8; 4096];
    let count =
        unsafe { proc_pidpath(pid.try_into()?, path.as_mut_ptr().cast(), path.len() as u32) };
    if count <= 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("could not read the executable of {pid}"));
    }
    let bytes = path
        .get(..usize::try_from(count)?)
        .context("incomplete executable path")?;
    Ok(Some(
        std::str::from_utf8(bytes)?
            .trim_end_matches('\0')
            .to_owned(),
    ))
}

// Every running process with this command name, in PID order, each with its birth. The
// command name is the executable's, whichever bundle it runs from and whoever owns it.
#[cfg(target_os = "macos")]
pub(in crate::native) fn macos_processes_named(
    command: &[u8],
) -> Result<Vec<MacTerminalAppIdentity>> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
    let mut processes = macos_process_records(&mut name)?
        .iter()
        .filter(|record| record.command.split(|byte| *byte == 0).next() == Some(command))
        .map(MacKinfoProc::identity)
        .collect::<Result<Vec<_>>>()?;
    processes.sort_by_key(|process| process.pid);
    Ok(processes)
}
