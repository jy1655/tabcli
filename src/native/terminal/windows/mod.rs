use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, INPUT_RECORD,
    KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CreateProcessW, GetProcessTimes, OpenProcess,
        PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        STARTUPINFOW,
    },
};

use super::{CloseOutcome, TerminalKind, TerminalSession, WindowsProcessIdentity};

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
    let command_line = format!("\"{powershell_text}\" -NoLogo -NoProfile -Command \"{command}\"");
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
    let identity = query_process_identity_from_handle(process.hProcess)?;
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
        windows_process_identity: Some(identity),
    })
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

pub(super) fn set_private_permissions(path: &Path, directory: bool) -> Result<()> {
    let system_directory = system_directory()?;
    let icacls = system_directory.join("icacls.exe");
    if !icacls.is_file() {
        bail!("Windows ACL tool was not found at {}", icacls.display());
    }
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
    let inheritance = if directory { "(OI)(CI)" } else { "" };
    let identity = format!("*{sid}:{inheritance}F");
    let output = Command::new(icacls)
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(identity)
        .output()
        .with_context(|| {
            format!(
                "failed to apply a private Windows ACL to {}",
                path.display()
            )
        })?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "failed to apply a private Windows ACL to {}{}",
            path.display(),
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        );
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
    verify_process_identity(pid, identity.creation_time, &identity.executable_path)?;
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
            let path = input_path.context("send requires a prompt path")?;
            let input = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read prompt payload {}", path.display()))?;
            write_console_input(&input, submit_count)
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

pub(super) fn query_process_identity(pid: u32) -> Result<WindowsProcessIdentity> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open Windows process {pid}"));
    }
    let result = query_process_identity_from_handle(handle);
    unsafe { CloseHandle(handle) };
    result
}

fn query_process_identity_from_handle(
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

pub(super) fn verify_process_identity(
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
        let first_submit = usize::from(submit_count > 0);
        write_input_records(handle, &build_console_input_records(input, first_submit))?;
        for _ in first_submit..submit_count {
            // Codex first confirms a bracketed paste and only then returns to the
            // composer. A separately timed Return is required to submit it.
            std::thread::sleep(std::time::Duration::from_millis(150));
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
mod tests {
    use super::{
        build_console_input_records, query_process_identity, resolve_executable_from_path,
        verify_process_identity,
    };
    use std::fs;

    #[test]
    fn powershell_resolution_uses_only_absolute_path_entries() {
        let directory = tempfile::tempdir().unwrap();
        let relative = directory.path().join("relative");
        let trusted = directory.path().join("trusted");
        fs::create_dir_all(&relative).unwrap();
        fs::create_dir_all(&trusted).unwrap();
        fs::write(relative.join("pwsh.exe"), b"planted").unwrap();
        fs::write(trusted.join("pwsh.exe"), b"trusted").unwrap();

        let path =
            std::env::join_paths([std::path::Path::new("relative"), trusted.as_path()]).unwrap();
        assert_eq!(
            resolve_executable_from_path("pwsh.exe", &path).unwrap(),
            trusted.join("pwsh.exe").canonicalize().unwrap()
        );
    }

    #[test]
    fn process_identity_rejects_reused_pid_creation_time() {
        let pid = std::process::id();
        let identity = query_process_identity(pid).unwrap();
        verify_process_identity(pid, identity.creation_time, &identity.executable_path).unwrap();
        assert!(
            verify_process_identity(
                pid,
                identity.creation_time.wrapping_add(1),
                &identity.executable_path,
            )
            .is_err()
        );
    }

    #[test]
    fn submit_is_a_real_windows_return_key_event() {
        let records = build_console_input_records("prompt", 2);
        let down = unsafe { records[records.len() - 2].Event.KeyEvent };
        let up = unsafe { records[records.len() - 1].Event.KeyEvent };
        assert_eq!(down.bKeyDown, 1);
        assert_eq!(up.bKeyDown, 0);
        assert_eq!(down.wVirtualKeyCode, 0x0d);
        assert_eq!(down.wVirtualScanCode, 0x1c);
        assert_eq!(unsafe { down.uChar.UnicodeChar }, u16::from(b'\r'));
        assert_eq!(
            records
                .iter()
                .filter(|record| unsafe {
                    record.Event.KeyEvent.bKeyDown == 1
                        && record.Event.KeyEvent.wVirtualKeyCode == 0x0d
                })
                .count(),
            2,
            "bracketed paste confirmation and prompt submission require separate Return keys"
        );
    }

    #[test]
    fn single_submit_provider_gets_one_return_key() {
        let records = build_console_input_records("prompt", 1);
        assert_eq!(
            records
                .iter()
                .filter(|record| unsafe {
                    record.Event.KeyEvent.bKeyDown == 1
                        && record.Event.KeyEvent.wVirtualKeyCode == 0x0d
                })
                .count(),
            1
        );
    }
}
