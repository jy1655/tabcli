use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleProcessList, GetConsoleWindow, INPUT_RECORD, KEY_EVENT,
    KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    },
    System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateProcessW,
        OpenProcess, PROCESS_INFORMATION, PROCESS_TERMINATE, ResumeThread, STARTUPINFOW,
        TerminateProcess, WaitForSingleObject,
    },
};

use super::{CloseOutcome, TerminalKind, TerminalSession, WindowsProcessIdentity};

mod process;
mod security;
pub(super) use process::{query_process_identity, verify_process_identity};
use process::{query_process_identity_from_handle, verify_control_process_identity};
pub(super) use security::set_private_permissions;

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    match preferred {
        None | Some(TerminalKind::WindowsConsole) => Ok(TerminalKind::WindowsConsole),
        Some(kind) => bail!(
            "{} is not available on Windows; use windows-console",
            kind.display_name()
        ),
    }
}

pub(super) fn open_bound_tab<F>(
    kind: TerminalKind,
    command: &str,
    bind: F,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
{
    if kind != TerminalKind::WindowsConsole {
        bail!("{} is not available on Windows", kind.display_name());
    }
    if command.contains('"') {
        bail!("Windows console launch command contains an unsupported double quote");
    }
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let powershell = resolve_executable_from_path("pwsh.exe", &path)
        .context("PowerShell 7 (pwsh.exe) was not found on an absolute PATH entry")?;
    let powershell_text = powershell.to_string_lossy();
    if powershell_text.contains('"') {
        bail!("PowerShell 7 executable path contains an unsupported double quote");
    }
    let application = powershell
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let console_command = console_command_line(command);
    let command_line =
        format!("\"{powershell_text}\" -NoLogo -NoProfile -Command \"{console_command}\"");
    let mut command_line = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            console_creation_flags(),
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open a managed Windows console with PowerShell 7 (pwsh.exe)");
    }
    let identity = match query_process_identity_from_handle(process.hProcess) {
        Ok(identity) => identity,
        Err(error) => {
            unsafe {
                TerminateProcess(process.hProcess, 1);
                CloseHandle(process.hThread);
                CloseHandle(process.hProcess);
            }
            return Err(error).context("failed to attest the managed Windows console process");
        }
    };
    let mut session = TerminalSession {
        kind,
        id: process.dwProcessId.to_string(),
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        windows_process_identity: Some(identity),
    };
    let process_handle = process.hProcess;
    let thread_handle = process.hThread;
    let launch = super::bind_suspended_surface_before_start(
        &mut session,
        bind,
        || {
            if unsafe { ResumeThread(thread_handle) } == u32::MAX {
                return Err(std::io::Error::last_os_error())
                    .context("failed to start the attested managed Windows console process");
            }
            Ok(())
        },
        || unsafe {
            TerminateProcess(process_handle, 1);
        },
    );
    unsafe {
        CloseHandle(thread_handle);
        CloseHandle(process_handle);
    }
    launch.context("failed to bind the suspended Windows console before startup")?;
    Ok(session)
}

fn console_command_line(command: &str) -> String {
    format!("Remove-Item Env:TERM -ErrorAction SilentlyContinue; {command}")
}

fn console_creation_flags() -> u32 {
    CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED
}

fn resolve_executable_from_path(name: &str, path: &OsStr) -> Result<PathBuf> {
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(name);
        if candidate.is_file() {
            return candidate.canonicalize().with_context(|| {
                format!("failed to canonicalize executable {}", candidate.display())
            });
        }
    }
    bail!("{name} was not found on an absolute PATH entry")
}

pub(super) fn powershell_executable() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_executable_from_path("pwsh.exe", &path)
        .context("PowerShell 7 (pwsh.exe) was not found on an absolute PATH entry")
}

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")?;
    run_console_helper("send", session, Some(prompt_path))
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    match run_console_helper("close", session, None) {
        Ok(()) => Ok(CloseOutcome::Closed),
        Err(error) if super::windows_console_helper_reports_missing(&error.to_string()) => {
            Ok(CloseOutcome::Missing)
        }
        Err(error) => Err(error),
    }
}

fn run_console_helper(action: &str, session: &TerminalSession, input: Option<&str>) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let managed_session_id = session
        .managed_session_id
        .as_deref()
        .context("Windows console handle is missing its managed session binding")?;
    let mut command = Command::new(executable);
    command.args(["native-console-control", action, managed_session_id]);
    if let Some(input) = input {
        let input_name = Path::new(input)
            .file_name()
            .context("prompt path has no file name")?;
        command.arg(input_name);
    }
    let output = command
        .output()
        .context("failed to start Windows console control helper")?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!(
        "{}",
        if message.is_empty() {
            "Windows console control helper failed"
        } else {
            &message
        }
    )
}

pub(super) fn console_control(
    action: &str,
    session: &TerminalSession,
    input_path: Option<&Path>,
    submit_count: usize,
) -> Result<()> {
    let pid = session
        .id
        .parse::<u32>()
        .context("Windows console session id is not a process id")?;
    let identity = session
        .windows_process_identity
        .as_ref()
        .context("Windows console handle is missing its process identity")?;
    verify_control_process_identity(pid, identity)?;
    // This runs only in the short-lived helper process so detaching its inherited
    // console cannot disturb the user's invoking PowerShell or cmd session.
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            bail!(
                "failed to attach to the managed console process: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    let close_requested = action == "close";
    let mut console_processes = Vec::new();
    let result = match action {
        "send" => {
            let path = input_path.context("send requires a prompt path")?;
            let input = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read prompt payload {}", path.display()))?;
            write_console_input(&input, submit_count)
        }
        "close" => {
            console_processes = attached_console_processes()?;
            let window = unsafe { GetConsoleWindow() };
            if !window.is_null() {
                // Closing the surface is best-effort. Conhost can invalidate the HWND
                // before the console root exits, so the identity-bound process fallback
                // below remains authoritative.
                unsafe { PostMessageW(window, console_close_message(), 0, 0) };
            }
            Ok(())
        }
        _ => bail!("unsupported native console action: {action}"),
    };
    unsafe { FreeConsole() };
    result?;
    if close_requested {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if verify_control_process_identity(pid, identity).is_ok() {
            terminate_console_processes(&console_processes, pid)?;
        }
        wait_for_console_process_exit(pid, identity)
    } else {
        Ok(())
    }
}

struct ConsoleProcess {
    pid: u32,
    handle: OwnedHandle,
}

fn attached_console_processes() -> Result<Vec<ConsoleProcess>> {
    let mut processes = vec![0u32; 64];
    loop {
        let count =
            unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) };
        if count == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to enumerate managed Windows console processes");
        }
        if count as usize <= processes.len() {
            processes.truncate(count as usize);
            let current_pid = std::process::id();
            return processes
                .into_iter()
                .filter(|pid| *pid != current_pid)
                .filter_map(|pid| {
                    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
                    if handle.is_null() {
                        if agent_bridge::process_is_alive(pid) {
                            return Some(Err(std::io::Error::last_os_error()).with_context(|| {
                                format!("failed to retain managed console process {pid}")
                            }));
                        }
                        return None;
                    }
                    Some(Ok(ConsoleProcess {
                        pid,
                        handle: unsafe { OwnedHandle::from_raw_handle(handle) },
                    }))
                })
                .collect();
        }
        processes.resize(count as usize, 0);
    }
}

fn terminate_console_processes(processes: &[ConsoleProcess], root_pid: u32) -> Result<()> {
    let ordered = processes
        .iter()
        .filter(|process| process.pid != root_pid)
        .chain(processes.iter().filter(|process| process.pid == root_pid));
    for process in ordered {
        let handle = process.handle.as_raw_handle();
        if unsafe { WaitForSingleObject(handle, 0) }
            == windows_sys::Win32::Foundation::WAIT_OBJECT_0
        {
            continue;
        }
        let terminated = unsafe { TerminateProcess(handle, 1) };
        let error = std::io::Error::last_os_error();
        if terminated == 0
            && unsafe { WaitForSingleObject(handle, 0) }
                != windows_sys::Win32::Foundation::WAIT_OBJECT_0
        {
            return Err(error)
                .with_context(|| format!("failed to terminate console process {}", process.pid));
        }
    }
    Ok(())
}

fn console_close_message() -> u32 {
    WM_CLOSE
}

fn wait_for_console_process_exit(pid: u32, identity: &WindowsProcessIdentity) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match verify_control_process_identity(pid, identity) {
            Err(_) if !agent_bridge::process_is_alive(pid) => return Ok(()),
            Err(error) => return Err(error).context("managed Windows console identity changed"),
            Ok(()) if std::time::Instant::now() >= deadline => {
                bail!("managed Windows console did not close within 5 seconds")
            }
            Ok(()) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

fn write_console_input(input: &str, submit_count: usize) -> Result<()> {
    let console_name = "CONIN$\0".encode_utf16().collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            console_name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("failed to open managed Windows console input");
    }
    let result = (|| {
        let first_submit = super::windows_console_immediate_submit_count(submit_count);
        write_input_records(handle, &build_console_input_records(input, first_submit))?;
        for _ in first_submit..submit_count {
            // Codex detects the fast synthetic text batch as a paste. Keep both
            // its confirmation Return and later submission Return out of that
            // batch so processing speed cannot decide which action they perform.
            std::thread::sleep(super::windows_console_extra_submit_delay());
            write_input_records(handle, &build_console_input_records("", 1))?;
        }
        Ok(())
    })();
    unsafe { CloseHandle(handle) };
    result
}

fn write_input_records(
    handle: windows_sys::Win32::Foundation::HANDLE,
    records: &[INPUT_RECORD],
) -> Result<()> {
    for chunk in records.chunks(1024) {
        let mut written = 0;
        if unsafe { WriteConsoleInputW(handle, chunk.as_ptr(), chunk.len() as u32, &mut written) }
            == 0
        {
            let error = Err(std::io::Error::last_os_error())
                .context("failed to write managed Windows console input");
            return error;
        }
        if written != chunk.len() as u32 {
            bail!(
                "managed Windows console accepted only {written} of {} input events",
                chunk.len()
            );
        }
    }
    Ok(())
}

fn build_console_input_records(input: &str, submit_count: usize) -> Vec<INPUT_RECORD> {
    let mut records = Vec::new();
    for character in input.encode_utf16() {
        for key_down in [1, 0] {
            records.push(INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: windows_sys::Win32::System::Console::INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: key_down,
                        wRepeatCount: 1,
                        wVirtualKeyCode: 0,
                        wVirtualScanCode: 0,
                        uChar: KEY_EVENT_RECORD_0 {
                            UnicodeChar: character,
                        },
                        dwControlKeyState: 0,
                    },
                },
            });
        }
    }
    for _ in 0..submit_count {
        for key_down in [1, 0] {
            records.push(INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: windows_sys::Win32::System::Console::INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: key_down,
                        wRepeatCount: 1,
                        wVirtualKeyCode: 0x0d,
                        wVirtualScanCode: 0x1c,
                        uChar: KEY_EVENT_RECORD_0 {
                            UnicodeChar: u16::from(b'\r'),
                        },
                        dwControlKeyState: 0,
                    },
                },
            });
        }
    }
    records
}

#[cfg(test)]
mod tests;
