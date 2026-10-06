pub(in crate::native) mod ownership;

use crate::native::session::{Reader, RecordStore, Store};
use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleProcessList, GetConsoleWindow, INPUT_RECORD, KEY_EVENT,
    KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_SHOWNOACTIVATE, WM_CLOSE};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    },
    System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, PROC_THREAD_ATTRIBUTE_JOB_LIST,
        PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, ResumeThread,
        STARTF_USESHOWWINDOW, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    },
};

use super::{
    CloseOutcome, TerminalKind, TerminalSession, WindowsProcessIdentity,
    WindowsProcessIdentityCheck,
};

mod focus;
mod process;
mod security;
mod tab;
pub(super) use process::{check_process_identity, query_process_identity, verify_process_identity};
use process::{
    open_verified_control_process, query_process_identity_from_handle,
    verify_control_process_identity,
};
pub(super) use security::set_private_permissions;
pub(super) use tab::run_host as run_console_host;

const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);
// A window that this launch brought to the front can take the foreground again while it
// starts (measured 2026-10-01, Windows Terminal 1.24: 54 ms after the first time), so the
// launch keeps answering for this long after the surface has started.
const FOREGROUND_SETTLE: Duration = Duration::from_millis(600);
// How often the window lookup looks again while the started root reaches its console and
// the terminal takes the console's window.
const CONSOLE_WINDOW_POLL: Duration = Duration::from_millis(5);
// How long a close waits for a root that has not reached its console, before it takes
// the root for one that was never started.
const CLOSE_ATTACH_WAIT: Duration = Duration::from_secs(1);
// How long a close gives the host of a tab to end on its own after the root has ended,
// and how long it then waits for a host it had to end.
const TAB_HOST_EXIT_WAIT: Duration = Duration::from_secs(1);

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
    directory: &Path,
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
    let powershell = powershell_executable()?;
    let command_line = console_launch_command_line(&powershell, command)?;
    // The window is the user's setting. One that cannot be read is no reason to open the
    // tab in a window they work in, which only an explicit choice does.
    let tab_window = directory
        .parent()
        .context("the session directory has no state root")
        .and_then(super::super::settings::windows_tab_window)
        .unwrap_or_else(|error| {
            super::super::launch::log(
                &Store::open_unchecked(directory),
                &format!("settings could not be read, using the dedicated window: {error:#}"),
            );
            super::super::settings::WindowsTabWindow::default()
        });
    // Watches the foreground from here until the surface has settled, and stops when it
    // is dropped on any earlier return.
    let guard = focus::ForegroundGuard::capture();
    // A tab of Windows Terminal is the preferred surface. A console window of its own
    // is created only when no tab can be had; the request for the tab is withdrawn
    // before that, and a surface is bound only once, so the session never runs in two.
    let surface = match tab::offer(
        &powershell,
        &command_line,
        directory,
        tab_window,
        startup_deadline,
        guard.as_ref(),
    ) {
        Ok(tab) => {
            super::super::launch::log(
                &Store::open_unchecked(directory),
                &format!(
                    "surface=windows-terminal-tab window={}",
                    tab_window.as_str()
                ),
            );
            Surface::Tab(tab)
        }
        Err(reason) => {
            super::super::launch::log(
                &Store::open_unchecked(directory),
                &format!("surface=console-window; no Windows Terminal tab: {reason:#}"),
            );
            // The session has no tab host. What an abandoned one left must be readable
            // or gone before a surface is created: a close would not get past it.
            tab::forget_unreadable_host(directory)?;
            Surface::Window(ConsoleWindow::create(&powershell, &command_line, deadline)?)
        }
    };
    let mut session = TerminalSession {
        kind,
        id: surface.pid().to_string(),
        tab_id: None,
        window_id: None,
        managed_session_id: None,
        wezterm_mux: None,
        windows_process_identity: Some(surface.identity().clone()),
    };
    let launch = super::bind_surface_before_start(
        &mut session,
        bind,
        || surface.start(startup_deadline),
        || surface.cleanup(bounded_startup_cleanup_deadline(deadline)),
        unbind,
    );
    launch.context("failed to bind the suspended Windows console before startup")?;
    surface.finish();
    if let Some(guard) = guard {
        let owned = surface.host_window();
        // A tab reported its window with its offer. A console window has a window only
        // now that its root runs, and only a process inside the console can read it.
        let mut lookup = owned
            .is_none()
            .then(|| WindowLookup::start(&session, FOREGROUND_SETTLE));
        let summary = guard.settle(
            owned,
            || lookup.as_mut().and_then(WindowLookup::poll),
            FOREGROUND_SETTLE,
        );
        super::super::launch::log(&Store::open_unchecked(directory), &summary);
    }
    Ok(session)
}

// A control helper that attaches to the started console and reports the window that
// hosts it. Best effort, like everything about the foreground: a helper that cannot be
// started, fails, or is too late reports nothing, and the launch goes on.
struct WindowLookup {
    helper: Option<std::process::Child>,
}

impl WindowLookup {
    fn start(session: &TerminalSession, timeout: Duration) -> Self {
        use std::os::windows::process::CommandExt;
        let helper = console_helper_command("window", session, None)
            .ok()
            .and_then(|mut command| {
                command
                    .arg(timeout.as_millis().max(1).to_string())
                    // The helper needs no console of its own, and must never be given a
                    // window that would itself come to the front.
                    .creation_flags(DETACHED_PROCESS)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            });
        Self { helper }
    }

    fn poll(&mut self) -> Option<isize> {
        let status = self.helper.as_mut()?.try_wait().ok().flatten()?;
        let output = self.helper.take()?.wait_with_output().ok()?;
        if !status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<isize>()
            .ok()
            .filter(|window| *window != 0)
    }
}

impl Drop for WindowLookup {
    fn drop(&mut self) {
        // Not waited for: the launch has no time to spare for a helper that is too late.
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.kill();
        }
    }
}

// The surface that holds the suspended PowerShell root of a managed console: a tab that
// Windows Terminal created, or a console window that this process created.
enum Surface {
    Tab(tab::TabSurface),
    Window(ConsoleWindow),
}

impl Surface {
    fn pid(&self) -> u32 {
        match self {
            Self::Tab(tab) => tab.pid(),
            Self::Window(window) => window.process.pid,
        }
    }

    fn identity(&self) -> &WindowsProcessIdentity {
        match self {
            Self::Tab(tab) => tab.identity(),
            Self::Window(window) => &window.identity,
        }
    }

    // The terminal window that the surface itself reported as its host. A console window
    // reports none: it has a window only once its root runs.
    fn host_window(&self) -> Option<isize> {
        match self {
            Self::Tab(tab) => tab.host_window(),
            Self::Window(_) => None,
        }
    }

    fn start(&self, startup_deadline: Instant) -> Result<()> {
        match self {
            Self::Tab(tab) => tab.start(startup_deadline),
            Self::Window(window) => {
                if Instant::now() >= startup_deadline {
                    bail!("Windows console startup timed out before process resume");
                }
                window
                    .process
                    .resume()
                    .context("failed to start the attested managed Windows console process")
            }
        }
    }

    fn cleanup(&self, deadline: Instant) -> Result<()> {
        match self {
            Self::Tab(tab) => tab.cleanup(deadline),
            Self::Window(window) => {
                terminate_process_until(window.process.process.as_raw_handle(), deadline)
            }
        }
    }

    fn finish(&self) {
        if let Self::Tab(tab) = self {
            tab.finish();
        }
    }
}

struct ConsoleWindow {
    process: CreatedProcess,
    identity: WindowsProcessIdentity,
}

impl ConsoleWindow {
    fn create(powershell: &Path, command_line: &str, deadline: Instant) -> Result<Self> {
        let process = CreatedProcess::create(
            powershell,
            command_line,
            console_creation_flags(),
            &console_startup_info(),
            None,
        )
        .context("failed to open a managed Windows console with PowerShell 7 (pwsh.exe)")?;
        match query_process_identity_from_handle(process.process.as_raw_handle()) {
            Ok(identity) => Ok(Self { process, identity }),
            Err(error) => {
                match terminate_process_until(
                    process.process.as_raw_handle(),
                    bounded_startup_cleanup_deadline(deadline),
                ) {
                    Ok(()) => {
                        Err(error).context("failed to attest the managed Windows console process")
                    }
                    Err(cleanup_error) => Err(anyhow::anyhow!(
                        "failed to attest the managed Windows console process: {error:#}; exact process cleanup also failed: {cleanup_error:#}"
                    )),
                }
            }
        }
    }
}

// A process and its main thread as `CreateProcessW` returned them. Both handles are
// closed when the value is dropped; the process itself is not ended.
struct CreatedProcess {
    process: OwnedHandle,
    thread: OwnedHandle,
    pid: u32,
}

impl CreatedProcess {
    fn create(
        application: &Path,
        command_line: &str,
        creation_flags: u32,
        startup: &STARTUPINFOW,
        environment: Option<&[u16]>,
    ) -> std::io::Result<Self> {
        Self::create_with(
            application,
            command_line,
            creation_flags,
            startup,
            environment,
            None,
        )
    }

    // The process is a member of `job` from its creation, so no moment exists at which
    // it runs, or waits suspended, outside the job.
    fn create_in_job(
        application: &Path,
        command_line: &str,
        creation_flags: u32,
        startup: &STARTUPINFOW,
        environment: Option<&[u16]>,
        job: HANDLE,
    ) -> std::io::Result<Self> {
        Self::create_with(
            application,
            command_line,
            creation_flags,
            startup,
            environment,
            Some(job),
        )
    }

    fn create_with(
        application: &Path,
        command_line: &str,
        creation_flags: u32,
        startup: &STARTUPINFOW,
        environment: Option<&[u16]>,
        job: Option<HANDLE>,
    ) -> std::io::Result<Self> {
        let application = application
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut command_line = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let (mut creation_flags, environment) = match environment {
            Some(block) => (
                creation_flags | CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr().cast(),
            ),
            None => (creation_flags, std::ptr::null()),
        };
        // The job list is an attribute of the extended startup information. Both it and
        // the handle it points to must outlive the creation call.
        let jobs = job.map(|job| [job]);
        let attributes = jobs.as_ref().map(JobListAttribute::new).transpose()?;
        let mut extended = STARTUPINFOEXW {
            StartupInfo: *startup,
            lpAttributeList: std::ptr::null_mut(),
        };
        if let Some(attributes) = &attributes {
            extended.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            extended.lpAttributeList = attributes.list;
            creation_flags |= EXTENDED_STARTUPINFO_PRESENT;
        }
        let mut process = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                creation_flags,
                environment,
                std::ptr::null(),
                &extended.StartupInfo,
                &mut process,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            process: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
            thread: unsafe { OwnedHandle::from_raw_handle(process.hThread) },
            pid: process.dwProcessId,
        })
    }

    fn resume(&self) -> std::io::Result<()> {
        if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

// A process attribute list that holds one attribute: the jobs a new process is created
// in. It borrows the job handles, which the attribute points to.
struct JobListAttribute<'jobs> {
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
    // The memory of `list`. Pointer-sized elements keep it aligned as Windows expects.
    _buffer: Vec<usize>,
    _jobs: std::marker::PhantomData<&'jobs [HANDLE; 1]>,
}

impl<'jobs> JobListAttribute<'jobs> {
    fn new(jobs: &'jobs [HANDLE; 1]) -> std::io::Result<Self> {
        let mut size = 0usize;
        // The first call only reports the size, and fails by design.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let list = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Initialised: from here on `drop` deletes the list.
        let attribute = Self {
            list,
            _buffer: buffer,
            _jobs: std::marker::PhantomData,
        };
        if unsafe {
            UpdateProcThreadAttribute(
                attribute.list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                std::mem::size_of_val(jobs),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(attribute)
    }
}

impl Drop for JobListAttribute<'_> {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

fn console_command_line(command: &str) -> String {
    format!("Remove-Item Env:TERM -ErrorAction SilentlyContinue; {command}")
}

// The whole command line of the console root. The command is passed to PowerShell as one
// quoted `-Command` argument, so neither it nor the executable path may hold a double
// quote: it would end the argument.
pub(super) fn console_launch_command_line(powershell: &Path, command: &str) -> Result<String> {
    if command.contains('"') {
        bail!("Windows console launch command contains an unsupported double quote");
    }
    let powershell_text = powershell.to_string_lossy();
    if powershell_text.contains('"') {
        bail!("PowerShell 7 executable path contains an unsupported double quote");
    }
    Ok(format!(
        "\"{powershell_text}\" -NoLogo -NoProfile -Command \"{}\"",
        console_command_line(command)
    ))
}

fn console_creation_flags() -> u32 {
    CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED
}

// Asks for a window that is shown without being activated. Windows Terminal honours it
// for a window that `wt.exe` creates while another application is in front (measured
// 2026-10-01, Windows Terminal 1.24). It does not when one of its own windows is in
// front, nor for a console that Windows hands to it as the default terminal, which is
// created in front, nor for a window that already exists, which it brings to the front;
// those are what `focus::ForegroundGuard` answers.
fn console_startup_info() -> STARTUPINFOW {
    STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: SW_SHOWNOACTIVATE as u16,
        ..Default::default()
    }
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
    let directory = Reader::session_directory(
        session
            .managed_session_id
            .as_deref()
            .context("missing managed session identity")?,
    )?;
    let mut file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(directory)?;
    RecordStore::set_file_private(file.as_file())?;
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
    records: &super::WindowsSessionRecords<'_>,
) -> Result<()> {
    let pid = session
        .id
        .parse::<u32>()
        .context("Windows console session id is not a process id")?;
    let identity = session
        .windows_process_identity
        .as_ref()
        .context("Windows console handle is missing its process identity")?;
    let close_requested = action == "close";
    // Read before anything is ended: a close that cannot tell the host of a tab from the
    // session's processes ends nothing and says why.
    let recorded_host = match close_requested {
        true => tab::recorded_host(records.directory)?,
        false => None,
    };
    let retained_owner = match open_verified_control_process(pid, identity) {
        Ok(owner) => owner,
        Err(error) => {
            // The root is gone, and with it the session. The host of its tab has nothing
            // left to wait for.
            if close_requested && super::windows_console_helper_reports_missing(&error.to_string())
            {
                end_tab_host(recorded_host.as_ref())?;
            }
            return Err(error);
        }
    };
    let window_deadline = match action {
        "window" => Some(
            Instant::now()
                .checked_add(timeout.context("window requires a console-control timeout")?)
                .context("Windows console window timeout is too large")?,
        ),
        _ => None,
    };
    // Two actions come when the root may not have reached its console: the lookup of the
    // window right after the root was started, and the close of a surface whose root was
    // never started. They wait for the root; every other action finds it attached.
    let attach_deadline =
        window_deadline.or_else(|| close_requested.then(|| Instant::now() + CLOSE_ATTACH_WAIT));
    // This runs only in the short-lived helper process so detaching its inherited
    // console cannot disturb the user's invoking PowerShell or cmd session.
    unsafe { FreeConsole() };
    while unsafe { AttachConsole(pid) } == 0 {
        let error = std::io::Error::last_os_error();
        if unsafe { WaitForSingleObject(retained_owner.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
            // The root ended while the close was coming. The host of its tab has
            // nothing left to wait for either.
            if close_requested {
                end_tab_host(recorded_host.as_ref())?;
            }
            bail!("console process is no longer available");
        }
        if attach_deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            if close_requested && records.root_never_ran {
                // The root has reached no console, and the wrapper it would start never
                // recorded itself: it is as it was created, suspended, with nothing
                // started. Ending it, and the host that never started it, is ending
                // everything.
                terminate_by_identity(pid, identity, Instant::now() + CLOSE_ATTACH_WAIT)
                    .context("failed to end the managed console root that was never started")?;
                return end_tab_host(recorded_host.as_ref());
            }
            bail!("failed to attach to the managed console process: {error}");
        }
        std::thread::sleep(CONSOLE_WINDOW_POLL);
    }

    let mut console_processes = Vec::new();
    let mut tab_host = None;
    let result = match action {
        "screen" => {
            println!("{}", serde_json::to_string(&attached_screen()?)?);
            Ok(())
        }
        "dialog" => {
            let path = input_path.context("dialog requires an input record")?;
            let input: super::GuardedDialogInput = serde_json::from_slice(
                &crate::native::session::RecordReader::at(path).raw_bytes()?,
            )?;
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
            let input = crate::native::session::RecordReader::at(path)
                .raw_text()
                .with_context(|| format!("failed to read prompt payload {}", path.display()))?;
            write_console_input(&input, submit_count, deadline)
        }
        "window" => {
            let deadline = window_deadline.context("window requires a console-control timeout")?;
            loop {
                if let Some(window) = focus::attached_console_host_window() {
                    println!("{window}");
                    break Ok(());
                }
                if Instant::now() >= deadline {
                    break Err(anyhow::anyhow!(
                        "the managed console has no window that hosts it"
                    ));
                }
                std::thread::sleep(CONSOLE_WINDOW_POLL);
            }
        }
        "close" => {
            console_processes = attached_console_processes()?;
            tab_host = tab_host_among(&console_processes, recorded_host.as_ref());
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
            terminate_console_processes(&console_processes, pid, tab_host)?;
        }
        wait_for_console_process_exit(pid, identity)?;
        end_tab_host(recorded_host.as_ref())
    } else {
        Ok(())
    }
}

// The host of a tab waits for the root and ends on its own, without a failure code, once
// the root has ended. One that does not is stalled, and its tab would stay open after
// the session is closed: it is ended here, without a failure code, so that Windows
// Terminal closes the tab. A host that has ended, or another process behind its pid, is
// left alone.
fn end_tab_host(recorded: Option<&(u32, WindowsProcessIdentity)>) -> Result<()> {
    let Some((pid, identity)) = recorded else {
        return Ok(());
    };
    // Looked at before any right to end it is asked for. A process behind the pid that
    // cannot even be looked at is not the host, which this user started: the host has
    // ended, and the pid belongs to something else now.
    let watched = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, *pid) };
    if watched.is_null() {
        return Ok(());
    }
    let watched = unsafe { OwnedHandle::from_raw_handle(watched) };
    let ended = |wait: Duration| {
        let waited =
            unsafe { WaitForSingleObject(watched.as_raw_handle(), wait.as_millis() as u32) };
        waited == WAIT_OBJECT_0
    };
    // Only a process that is positively the recorded host is ever ended here.
    let is_host = || {
        query_process_identity_from_handle(watched.as_raw_handle())
            .is_ok_and(|live| live == *identity)
    };
    if ended(Duration::ZERO) || !is_host() || ended(TAB_HOST_EXIT_WAIT) {
        return Ok(());
    }
    // The pid cannot be given to another process while `watched` is open, so this is
    // the same process.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, *pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!("failed to open the stalled Windows Terminal tab host {pid}")
        });
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let terminated = unsafe { TerminateProcess(handle.as_raw_handle(), 0) };
    let error = std::io::Error::last_os_error();
    if terminated == 0 && !ended(Duration::ZERO) {
        return Err(error)
            .with_context(|| format!("failed to end the stalled Windows Terminal tab host {pid}"));
    }
    if !ended(TAB_HOST_EXIT_WAIT) {
        bail!("the stalled Windows Terminal tab host {pid} did not end");
    }
    Ok(())
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
        {
            return Err(std::io::Error::last_os_error())
                .context("could not read the managed console screen");
        }
        text.push_str(&screen_row(&line, read)?);
        text.push('\n');
    }
    Ok(text)
}

// The console returns a full-width character once although it fills two cells, so a row
// with Korean text or an emoji holds fewer characters than cells. Only a count beyond the
// row is a failed read.
fn screen_row(cells: &[u16], read: u32) -> Result<String> {
    let characters = cells
        .get(..read as usize)
        .context("the managed console returned more characters than the screen row holds")?;
    Ok(String::from_utf16(characters)?)
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
                    let handle = unsafe {
                        OpenProcess(
                            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                            0,
                            pid,
                        )
                    };
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

// The host of a Windows Terminal tab creates the console root in the tab's console and
// sits in that console until the root has attached to it. A close that comes in that
// time finds it among the console's processes. It is the tab's own process: Windows
// Terminal keeps the tab open when it ends with a failure code.
//
// The host records its own pid and identity in the session directory. A process of the
// console is the host when it is exactly that process.
fn tab_host_among(
    processes: &[ConsoleProcess],
    recorded: Option<&(u32, WindowsProcessIdentity)>,
) -> Option<u32> {
    let (pid, identity) = recorded?;
    processes
        .iter()
        .find(|process| process.pid == *pid)
        .filter(|process| {
            query_process_identity_from_handle(process.handle.as_raw_handle())
                .is_ok_and(|live| live == *identity)
        })
        .map(|process| process.pid)
}

// Ends the process with this pid if it is still the process of this identity. A process
// that has ended, or another process behind the pid, is left alone.
fn terminate_by_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
    deadline: Instant,
) -> Result<()> {
    let handle: HANDLE = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        if !agent_bridge::process_is_alive(pid) {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to open the managed console root {pid}"));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    // An ended process can still be opened while a handle to it exists, but its identity
    // can no longer be read. A different process behind the pid means the same: the
    // root has already ended.
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0
        || query_process_identity_from_handle(handle.as_raw_handle())? != *identity
    {
        return Ok(());
    }
    terminate_process_until(handle.as_raw_handle(), deadline)
}

// The root goes after the processes it started, and the tab host last and without a
// failure code, so that the tab closes.
fn terminate_console_processes(
    processes: &[ConsoleProcess],
    root_pid: u32,
    tab_host: Option<u32>,
) -> Result<()> {
    let rank = |process: &ConsoleProcess| {
        if Some(process.pid) == tab_host {
            2
        } else if process.pid == root_pid {
            1
        } else {
            0
        }
    };
    let mut ordered = processes.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|process| rank(process));
    for process in ordered {
        let handle = process.handle.as_raw_handle();
        if unsafe { WaitForSingleObject(handle, 0) }
            == windows_sys::Win32::Foundation::WAIT_OBJECT_0
        {
            continue;
        }
        let exit_code = if Some(process.pid) == tab_host { 0 } else { 1 };
        let terminated = unsafe { TerminateProcess(handle, exit_code) };
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
