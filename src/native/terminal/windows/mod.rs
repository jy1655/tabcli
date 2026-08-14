use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, INPUT_RECORD,
    KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CreateProcessW, PROCESS_INFORMATION,
        STARTUPINFOW,
    },
};

use super::{CloseOutcome, TerminalKind, TerminalSession};

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    match preferred {
        None | Some(TerminalKind::WindowsConsole) => Ok(TerminalKind::WindowsConsole),
        Some(kind) => bail!(
            "{} is not available on Windows; use windows-console",
            kind.display_name()
        ),
    }
}

pub(super) fn open_tab(kind: TerminalKind, command: &str) -> Result<TerminalSession> {
    if kind != TerminalKind::WindowsConsole {
        bail!("{} is not available on Windows", kind.display_name());
    }
    if command.contains('"') {
        bail!("Windows console launch command contains an unsupported double quote");
    }
    let command_line = format!("pwsh.exe -NoLogo -NoProfile -Command \"{command}\"");
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
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP,
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
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(TerminalSession {
        kind,
        id: process.dwProcessId.to_string(),
        tab_id: None,
        window_id: None,
        managed_session_id: None,
    })
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
        Err(error)
            if error
                .to_string()
                .contains("console process is no longer available") =>
        {
            Ok(CloseOutcome::Missing)
        }
        Err(error) => Err(error),
    }
}

fn run_console_helper(action: &str, session: &TerminalSession, input: Option<&str>) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut command = Command::new(executable);
    command.args(["native-console-control", action, &session.id]);
    if let Some(input) = input {
        command.arg(input);
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

pub(super) fn console_control(action: &str, pid: u32, input: Option<&str>) -> Result<()> {
    // This runs only in the short-lived helper process so detaching its inherited
    // console cannot disturb the user's invoking PowerShell or cmd session.
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            bail!(
                "console process is no longer available: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    let result = match action {
        "send" => {
            let path = Path::new(input.context("send requires a prompt path")?);
            let input = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read prompt payload {}", path.display()))?;
            write_console_input(&input)
        }
        "close" => {
            if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
                Err(std::io::Error::last_os_error())
                    .context("failed to interrupt managed Windows console")
            } else {
                Ok(())
            }
        }
        _ => bail!("unsupported native console action: {action}"),
    };
    unsafe { FreeConsole() };
    result
}

fn write_console_input(input: &str) -> Result<()> {
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
    let mut records = Vec::new();
    for character in input
        .encode_utf16()
        .chain(std::iter::once(u16::from(b'\r')))
    {
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
    for chunk in records.chunks(1024) {
        let mut written = 0;
        if unsafe { WriteConsoleInputW(handle, chunk.as_ptr(), chunk.len() as u32, &mut written) }
            == 0
        {
            let error = Err(std::io::Error::last_os_error())
                .context("failed to write managed Windows console input");
            unsafe { CloseHandle(handle) };
            return error;
        }
        if written != chunk.len() as u32 {
            bail!(
                "managed Windows console accepted only {written} of {} input events",
                chunk.len()
            );
        }
    }
    unsafe { CloseHandle(handle) };
    Ok(())
}
