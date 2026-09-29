use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleProcessList, GetConsoleWindow, INPUT_RECORD, KEY_EVENT,
    KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    },
    System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateProcessW,
        OpenProcess, PROCESS_INFORMATION, PROCESS_TERMINATE, ResumeThread, STARTUPINFOW,
        TerminateProcess, WaitForSingleObject,
    },
};

use super::{
    CloseOutcome, TerminalKind, TerminalSession, WindowsProcessIdentity,
    WindowsProcessIdentityCheck,
};

mod process;
mod security;
pub(super) use process::{check_process_identity, query_process_identity, verify_process_identity};
use process::{
    open_verified_control_process, query_process_identity_from_handle,
    verify_control_process_identity,
};
pub(super) use security::set_private_permissions;

const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    match preferred {
        None | Some(TerminalKind::WindowsConsole) => Ok(TerminalKind::WindowsConsole),
        Some(kind) => bail!(
            "{} is not available on Windows; use windows-console",
            kind.display_name()
        ),
    }
}

pub(super) fn open_bound_tab<F, U>(
    kind: TerminalKind,
    command: &str,
    deadline: Instant,
    bind: F,
    unbind: U,
) -> Result<TerminalSession>
where
    F: FnOnce(&mut TerminalSession) -> Result<()>,
    U: FnOnce() -> Result<()>,
{
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .context("Windows console startup timeout leaves no room for exact process cleanup")?;
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
            let cleanup = terminate_process_until(
                process.hProcess,
                bounded_startup_cleanup_deadline(deadline),
            );
            unsafe {
                CloseHandle(process.hThread);
                CloseHandle(process.hProcess);
            }
            return match cleanup {
                Ok(()) => {
                    Err(error).context("failed to attest the managed Windows console process")
                }
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "failed to attest the managed Windows console process: {error:#}; exact process cleanup also failed: {cleanup_error:#}"
                )),
            };
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
    let launch = super::bind_surface_before_start(
        &mut session,
        bind,
        || {
            if Instant::now() >= startup_deadline {
                bail!("Windows console startup timed out before process resume");
            }
            if unsafe { ResumeThread(thread_handle) } == u32::MAX {
                return Err(std::io::Error::last_os_error())
                    .context("failed to start the attested managed Windows console process");
            }
            Ok(())
        },
        || terminate_process_until(process_handle, bounded_startup_cleanup_deadline(deadline)),
        unbind,
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

fn bounded_startup_cleanup_deadline(deadline: Instant) -> Instant {
    Instant::now()
        .checked_add(STARTUP_CLEANUP_RESERVE)
        .map_or(deadline, |candidate| candidate.min(deadline))
}

fn terminate_process_until(handle: HANDLE, deadline: Instant) -> Result<()> {
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => return Ok(()),
        WAIT_FAILED => {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect the managed Windows console during cleanup");
        }
        _ => {}
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    let terminate_error = (terminated == 0).then(std::io::Error::last_os_error);
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("managed Windows console cleanup exhausted its deadline")?;
    let timeout_ms = remaining
        .as_millis()
        .saturating_add(1)
        .min(u128::from(u32::MAX - 1)) as u32;
    let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if wait == WAIT_OBJECT_0 {
        return Ok(());
    }
    let wait_error = match wait {
        WAIT_TIMEOUT => anyhow::anyhow!("managed Windows console cleanup timed out"),
        WAIT_FAILED => anyhow::Error::new(std::io::Error::last_os_error())
            .context("failed to wait for managed Windows console cleanup"),
        other => anyhow::anyhow!("unexpected Windows console cleanup wait result {other}"),
    };
    match terminate_error {
        Some(error) => Err(anyhow::anyhow!(
            "failed to terminate the managed Windows console: {error}; termination remained unconfirmed: {wait_error:#}"
        )),
        None => Err(wait_error),
    }
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

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> super::TerminalSendResult {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(super::TerminalSendFailure::not_sent)?;
    run_console_send_helper(session, prompt_path, deadline)
}

pub(super) fn read_screen(session: &TerminalSession, deadline: Instant) -> Result<String> {
    let mut command = console_helper_command("screen", session, None)?;
    let output =
        super::super::command_output_until(&mut command, deadline, "managed console screen")?;
    if !output.status.success() {
        bail!("{}", console_helper_failure_message(&output));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

pub(super) fn guarded_dialog_input(
    session: &TerminalSession,
    input: &super::GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    use std::io::Write;
    let directory = super::super::session_directory(
        session
            .managed_session_id
            .as_deref()
            .context("missing managed session identity")?,
    )?;
    let mut file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(directory)?;
    super::super::set_private_file_permissions(file.as_file())?;
    file.write_all(&serde_json::to_vec(input)?)?;
    file.flush()?;
    let mut command = console_helper_command(
        "dialog",
        session,
        Some(file.path().to_str().context("dialog path is not UTF-8")?),
    )?;
    command.arg(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1)
            .to_string(),
    );
    let output =
        super::super::command_output_until(&mut command, deadline, "managed console dialog input")?;
    if !output.status.success() {
        bail!("{}", console_helper_failure_message(&output));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "sent" => Ok(true),
        "changed" => Ok(false),
        _ => bail!("unexpected managed console dialog result"),
    }
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

fn run_console_send_helper(
    session: &TerminalSession,
    input: &str,
    deadline: Instant,
) -> super::TerminalSendResult {
    let mut command = console_helper_command("send", session, Some(input))
        .map_err(super::TerminalSendFailure::not_sent)?;
    let timeout = super::remaining_send_budget_at(deadline, Instant::now())
        .map_err(super::TerminalSendFailure::not_sent)?;
    let timeout_ms = u64::try_from(timeout.as_millis())
        .context("Windows console timeout is too large")
        .map_err(super::TerminalSendFailure::not_sent)?;
    command.arg(timeout_ms.max(1).to_string());
    let output = super::super::command_output_until_classified(
        &mut command,
        deadline,
        "Windows console control helper",
    )
    .map_err(|failure| {
        if failure.process_started() {
            super::TerminalSendFailure::delivery_uncertain(failure.into_error())
        } else {
            super::TerminalSendFailure::not_sent(failure.into_error())
        }
    })?;
    if output.status.success() {
        return Ok(());
    }
    let message = console_helper_failure_message(&output);
    let error = anyhow::anyhow!(message);
    if super::windows_console_helper_reports_send_not_started(&error.to_string()) {
        Err(super::TerminalSendFailure::not_sent(error))
    } else {
        Err(super::TerminalSendFailure::delivery_uncertain(error))
    }
}

fn run_console_helper(action: &str, session: &TerminalSession, input: Option<&str>) -> Result<()> {
    let mut command = console_helper_command(action, session, input)?;
    let output = command
        .output()
        .context("failed to start Windows console control helper")?;
    if output.status.success() {
        return Ok(());
    }
    bail!("{}", console_helper_failure_message(&output))
}

fn console_helper_command(
    action: &str,
    session: &TerminalSession,
    input: Option<&str>,
) -> Result<Command> {
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
    Ok(command)
}

fn console_helper_failure_message(output: &std::process::Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        "Windows console control helper failed".to_owned()
    } else {
        message
    }
}

pub(super) fn console_control(
    action: &str,
    session: &TerminalSession,
    input_path: Option<&Path>,
    submit_count: usize,
    timeout: Option<Duration>,
) -> Result<()> {
    let pid = session
        .id
        .parse::<u32>()
        .context("Windows console session id is not a process id")?;
    let identity = session
        .windows_process_identity
        .as_ref()
        .context("Windows console handle is missing its process identity")?;
    let _retained_owner = open_verified_control_process(pid, identity)?;
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
        "screen" => {
            println!("{}", serde_json::to_string(&attached_screen()?)?);
            Ok(())
        }
        "dialog" => {
            let path = input_path.context("dialog requires an input record")?;
            let input: super::GuardedDialogInput = serde_json::from_slice(&std::fs::read(path)?)?;
            if input.screen != attached_screen()? {
                println!("changed");
            } else {
                write_dialog_key(input.key)?;
                println!("sent");
            }
            Ok(())
        }
        "send" => {
            let timeout = timeout.context("send requires a console-control timeout")?;
            if !super::windows_console_submit_delays_fit(submit_count, timeout) {
                bail!(
                    "Windows console submission delays do not fit inside the remaining turn timeout"
                );
            }
            let deadline = Instant::now()
                .checked_add(timeout)
                .context("Windows console input timeout is too large")?;
            let path = input_path.context("send requires a prompt path")?;
            let input = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read prompt payload {}", path.display()))?;
            write_console_input(&input, submit_count, deadline)
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

fn attached_screen() -> Result<String> {
    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, COORD, GetConsoleScreenBufferInfo,
            ReadConsoleOutputCharacterW,
        },
    };
    let name: Vec<u16> = "CONOUT$".encode_utf16().chain(Some(0)).collect();
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let width = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
    let height = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
    if width <= 0 || height <= 0 || width * height > 131072 {
        bail!("invalid managed console screen dimensions");
    }
    let mut text = String::new();
    for y in info.srWindow.Top..=info.srWindow.Bottom {
        let mut line = vec![0u16; width as usize];
        let mut read = 0;
        if unsafe {
            ReadConsoleOutputCharacterW(
                handle.as_raw_handle(),
                line.as_mut_ptr(),
                width as u32,
                COORD {
                    X: info.srWindow.Left,
                    Y: y,
                },
                &mut read,
            )
        } == 0
            || read != width as u32
        {
            bail!("could not read the entire managed console screen");
        }
        text.push_str(&String::from_utf16(&line)?);
        text.push('\n');
    }
    Ok(text)
}

fn write_dialog_key(key: super::DialogKey) -> Result<()> {
    let name: Vec<u16> = "CONIN$".encode_utf16().chain(Some(0)).collect();
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut records = Vec::new();
    if key == super::DialogKey::DownEnter {
        for key_down in [1, 0] {
            records.push(INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: windows_sys::Win32::System::Console::INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: key_down,
                        wRepeatCount: 1,
                        wVirtualKeyCode: 0x28,
                        wVirtualScanCode: 0x50,
                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: 0 },
                        dwControlKeyState: 0,
                    },
                },
            });
        }
    }
    records.extend(build_console_input_records("", 1));
    write_input_records(handle.as_raw_handle(), &records)
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

fn write_console_input(input: &str, submit_count: usize, deadline: Instant) -> Result<()> {
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
        if Instant::now() >= deadline {
            bail!("Windows console input timed out before delivery");
        }
        write_input_records(handle, &build_console_input_records(input, first_submit))?;
        for _ in first_submit..submit_count {
            // Codex detects the fast synthetic text batch as a paste. Keep both
            // its confirmation Return and later submission Return out of that
            // batch so processing speed cannot decide which action they perform.
            let delay = super::windows_console_extra_submit_delay();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if delay >= remaining {
                bail!("Windows console input timed out before the next submission Return");
            }
            std::thread::sleep(delay);
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
