//! The Windows transport of the Herdr adapter: the core's named pipe, located as `hc` locates
//! it (`windows_endpoint.rs`), and this machine's processes.
//!
//! hc serves `\\.\pipe\herdr-core-<user>-<session>.sock` with a DACL that admits only
//! SYSTEM and its own user, rejects remote clients, and labels the pipe with its integrity
//! level so that a lower process cannot open an elevated core's pipe. hc itself checks no
//! server: it connects with `SECURITY_IDENTIFICATION` only. Bridge also connects so (a
//! server cannot act as Bridge's user), then requires the pipe's server process to run as
//! this user, from its token, as `SO_PEERCRED` and `getpeereid` prove on Unix. Every read
//! and write is overlapped and waited for no longer than the call's deadline; one that runs
//! out of time is cancelled and its completion collected before its buffer is released.
use super::windows_endpoint::{WindowsEndpointInputs, local_pipe_name, resolve_pipe};
use super::{Connection, Core, Framed, Stream, not_running};
use crate::native::terminal::record::HerdrHostBirth;
use anyhow::{Context, Result, bail};
use std::{
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
        ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED,
        GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_TYPE_PIPE, GetFileType, OPEN_EXISTING, ReadFile,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            GetNamedPipeInfo, GetNamedPipeServerProcessId, PIPE_SERVER_END, PIPE_TYPE_MESSAGE,
            WaitNamedPipeW,
        },
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

/// How long one wait for a busy pipe lasts before the deadline is looked at again.
const BUSY_WAIT: Duration = Duration::from_millis(100);

pub(in crate::native::terminal) struct Installed;

impl Core for Installed {
    fn endpoint(&self) -> Result<PathBuf> {
        resolve_pipe(
            &WindowsEndpointInputs::from_env(),
            &super::read_config_socket,
        )
    }

    fn connect(&self, socket: &Path, deadline: Instant) -> Result<Box<dyn Connection + '_>> {
        Ok(Box::new(open(socket, deadline)?))
    }

    fn executable(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed to resolve Agent Bridge executable")
    }

    fn caller_terminal(&self) -> Option<String> {
        super::caller_terminal_from_env()
    }

    fn current_pid(&self) -> u32 {
        std::process::id()
    }

    fn parent_pid(&self, pid: u32) -> Result<Option<u32>> {
        parent_of(pid)
    }

    fn process_start(&self, pid: u32) -> Result<Option<HerdrHostBirth>> {
        Ok(creation_time(pid)?
            .map(|host_creation_time| HerdrHostBirth::Windows { host_creation_time }))
    }

    fn launch_nonce(&self) -> Result<String> {
        let mut bytes = [0_u8; 16];
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            bail!("Windows could not generate a Herdr launch token (status {status:#x})");
        }
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

/// The creation time of a live process; `None` when it has exited or no process holds the
/// PID. An exited process whose object is still held is absent too (attestation boundary).
pub(super) fn creation_time(pid: u32) -> Result<Option<u64>> {
    Ok(
        crate::native::terminal::windows::observe_process_identity(pid)?
            .map(|identity| identity.creation_time),
    )
}

/// The parent of a live process. Windows keeps a child's parent PID after the parent ends,
/// and that PID can then name a later process: only a live process created no later than
/// the child is its parent. A parent this process may not inspect ends the walk; the walk
/// can then only refuse, never accept.
pub(super) fn parent_of(pid: u32) -> Result<Option<u32>> {
    let Some(parent) = snapshot_parent(pid)? else {
        return Ok(None);
    };
    let Some(child_created) = creation_time(pid)? else {
        return Ok(None);
    };
    match creation_time(parent) {
        Ok(Some(parent_created)) if parent_created <= child_created => Ok(Some(parent)),
        Ok(_) | Err(_) => Ok(None),
    }
}

/// The parent PID that a process snapshot records for `pid`, if the snapshot lists it.
fn snapshot_parent(pid: u32) -> Result<Option<u32>> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("cannot list Windows processes");
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    while more != 0 {
        if entry.th32ProcessID == pid {
            return Ok(
                (entry.th32ParentProcessID != 0 && entry.th32ParentProcessID != pid)
                    .then_some(entry.th32ParentProcessID),
            );
        }
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    Ok(None)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// One connection to the core's pipe: the client end of a byte pipe whose server process
/// runs as this user.
pub(super) fn open(pipe: &Path, deadline: Instant) -> Result<Framed<Pipe>> {
    local_pipe_name(pipe)?;
    let name = wide(pipe);
    let handle = loop {
        // Identification only: whoever serves this name cannot act as this user.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE_VALUE {
            break unsafe { OwnedHandle::from_raw_handle(raw) };
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_FILE_NOT_FOUND) => return Err(not_running(pipe)),
            // Every instance is taken; hc offers another as soon as it accepts one.
            Some(ERROR_PIPE_BUSY) => {
                let wait = super::remaining(deadline)
                    .map_err(|_| anyhow::anyhow!("Herdr connection timed out"))?
                    .min(BUSY_WAIT);
                unsafe { WaitNamedPipeW(name.as_ptr(), wait.as_millis().max(1) as u32) };
            }
            Some(ERROR_ACCESS_DENIED) => {
                return Err(error).with_context(|| {
                    format!(
                        "the Herdr Core pipe {} refuses this process: a core started elevated or as another user cannot be used; start hc in a normal desktop terminal of this user",
                        pipe.display()
                    )
                });
            }
            _ => {
                return Err(error).with_context(|| format!("cannot connect to {}", pipe.display()));
            }
        }
    };
    let pipe_display = pipe.display();
    if unsafe { GetFileType(handle.as_raw_handle()) } != FILE_TYPE_PIPE {
        bail!("{pipe_display} is not a named pipe");
    }
    let mut flags = 0;
    if unsafe {
        GetNamedPipeInfo(
            handle.as_raw_handle(),
            &mut flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot inspect the Herdr Core pipe {pipe_display}"));
    }
    if flags & PIPE_SERVER_END != 0 || flags & PIPE_TYPE_MESSAGE != 0 {
        bail!("{pipe_display} is not the client end of a byte pipe");
    }
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(handle.as_raw_handle(), &mut server) } == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot identify the process serving {pipe_display}"));
    }
    let own = crate::native::terminal::windows::process_user_sid()?;
    let served_by = crate::native::terminal::windows::process_user_sid_of(server)
        .with_context(|| format!("cannot identify the user serving {pipe_display}"))?;
    if !same_user(&own, &served_by) {
        bail!("the process serving {pipe_display} is not this user's");
    }
    if Instant::now() >= deadline {
        bail!("Herdr connection timed out");
    }
    Ok(Framed::new(Pipe::new(handle)?))
}

/// Whether two string SIDs name one account: SIDs compare without regard to case.
pub(super) fn same_user(own: &str, other: &str) -> bool {
    own.eq_ignore_ascii_case(other)
}

/// The client end of the core's pipe, with the event its overlapped operations signal.
pub(super) struct Pipe {
    handle: OwnedHandle,
    event: OwnedHandle,
}

impl Pipe {
    pub(super) fn new(handle: OwnedHandle) -> Result<Self> {
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error()).context("cannot create a pipe event");
        }
        Ok(Self {
            handle,
            event: unsafe { OwnedHandle::from_raw_handle(raw) },
        })
    }

    /// One overlapped read or write, waited for until `deadline`. One that is still pending
    /// then is cancelled, and its completion is collected before this returns, so that the
    /// buffer outlives the operation. What it transferred before the cancellation is
    /// reported; a cancelled operation that transferred nothing is a timeout. A pipe whose
    /// other end has gone reads as the end of the stream.
    fn transfer(
        &mut self,
        deadline: Instant,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> std::io::Result<usize> {
        let timeout = super::remaining(deadline)?;
        let handle = self.handle.as_raw_handle();
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.as_raw_handle();
        if start(handle, &mut overlapped) == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return closed_or(error);
            }
        }
        let millis = timeout.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32;
        let waited = unsafe { WaitForSingleObject(self.event.as_raw_handle(), millis) };
        if waited != WAIT_OBJECT_0 {
            // Cancels only this operation; one that completed meanwhile keeps its result.
            unsafe { CancelIoEx(handle, &overlapped) };
        }
        let mut transferred = 0;
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } == 0 {
            let error = unsafe { GetLastError() };
            return match error {
                ERROR_OPERATION_ABORTED if transferred > 0 => Ok(transferred as usize),
                ERROR_OPERATION_ABORTED if waited == WAIT_TIMEOUT => {
                    Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
                }
                error => closed_or(error),
            };
        }
        Ok(transferred as usize)
    }
}

fn closed_or(error: u32) -> std::io::Result<usize> {
    match error {
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED => Ok(0),
        error => Err(std::io::Error::from_raw_os_error(error as i32)),
    }
}

impl Stream for Pipe {
    fn write_before(&mut self, bytes: &[u8], deadline: Instant) -> std::io::Result<usize> {
        let length = bytes.len().min(u32::MAX as usize) as u32;
        self.transfer(deadline, |handle, overlapped| unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                length,
                std::ptr::null_mut(),
                overlapped,
            )
        })
    }

    fn read_before(&mut self, buffer: &mut [u8], deadline: Instant) -> std::io::Result<usize> {
        let length = buffer.len().min(u32::MAX as usize) as u32;
        self.transfer(deadline, |handle, overlapped| unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                length,
                std::ptr::null_mut(),
                overlapped,
            )
        })
    }
}
