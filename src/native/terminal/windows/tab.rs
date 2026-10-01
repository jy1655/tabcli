//! Hosts a managed console in a tab of the Agent Bridge window of Windows Terminal.
//!
//! `wt.exe` creates the process of a new tab itself, so the launcher cannot create the
//! console root suspended and attest it, as it does for a console window of its own. The
//! tab therefore runs this binary as `native-console-host`. The host creates the same
//! PowerShell root, suspended, in the tab's console, offers its attested identity through
//! the private session directory, and starts it only after the launcher has durably bound
//! the surface. The session then has the same root, command line and environment as in a
//! console window, and every later control path is unchanged.
//!
//! The tab goes to a window named `agent-bridge`, which Windows Terminal creates when it
//! does not exist. By default it never goes to a window the user works in: Windows
//! Terminal selects a new tab, cannot create one unselected, and offers no way to select
//! the earlier tab again, so a tab there takes the keyboard inside that window for good.
//! `settings windows-tab-window current` is the user's choice of exactly that.
//!
//! Two records, each with one writer, and one decision carry the handshake:
//!
//! - `console-host.json`, written by the launcher: the root to create and the attempt it
//!   belongs to. Rewritten with `accepted` once the surface is bound. Removing it cancels
//!   the attempt.
//! - `console-host-offer.json`, written by the host: the suspended root and the terminal
//!   window that hosts the tab; `started` once the root runs and the host has left the
//!   console; or the reason it refused.
//! - `console-host-decision`, created exactly once, by the host to start the root or by
//!   the launcher to give the tab up after it accepted the offer. Whoever creates it has
//!   decided; the other side finds it there.
//!
//! What can be left behind is settled by who is still there to end it:
//!
//! - From its creation until after it was started, the root is in a job that ends it when
//!   the host ends, so a host that dies leaves no process. A host that is alive ends the
//!   root itself when the request is cancelled or runs out of time.
//! - The launcher ends the offered root by its identity when it gives the tab up and the
//!   host has not decided to start the root: the decision keeps the host from starting it
//!   afterwards. When the host has decided, the root starts the session's wrapper and
//!   provider, and the launcher ends nothing: a host that has decided but not confirmed
//!   the start in time leaves a surface that counts as started, and the launch goes on
//!   to wait for the provider like every launch. A launch that fails there is fenced, so
//!   a wrapper that starts later starts no provider, and the surface stays bound to the
//!   session, where `close-session` finds the console by its root.
//! - A console window is created only after the request has been withdrawn. A host that
//!   arrives later, or notices later, never starts its root: until it has noticed, an
//!   empty tab can exist beside the console window, but the session is bound to, and
//!   started in, only one of them.
//!
//! Replace this with the creation and attestation of the root in the launcher when
//! Windows Terminal can adopt a process that its caller created.
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::WAIT_OBJECT_0,
    System::{
        Console::{FlushConsoleInputBuffer, FreeConsole, GetConsoleProcessList},
        JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, INFINITE, STARTUPINFOW, WaitForSingleObject,
        },
    },
    UI::WindowsAndMessaging::GetShellWindow,
};

use super::super::super::settings::WindowsTabWindow;
use super::{
    CreatedProcess, WindowsProcessIdentity, WindowsProcessIdentityCheck, check_process_identity,
    console_startup_info,
    focus::{ForegroundGuard, attached_console_host_window},
    query_process_identity, query_process_identity_from_handle, terminate_by_identity,
    terminate_process_until,
};

const WINDOW_NAME: &str = "agent-bridge";
const HOST_COMMAND: &str = "native-console-host";
const REQUEST_FILE: &str = "console-host.json";
const OFFER_FILE: &str = "console-host-offer.json";
const DECISION_FILE: &str = "console-host-decision";
// Written by the host about itself, and kept for as long as the session directory: a
// close reads it to know the host among the processes of the console.
const HOST_PROCESS_FILE: &str = "console-host-process.json";
// Set only for `wt.exe`. Windows Terminal passes the environment of that invocation to
// the tab, so a host that finds its attempt here runs in the launcher's environment.
const ATTEMPT_ENV: &str = "AGENT_BRIDGE_CONSOLE_HOST_ATTEMPT";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
// How long a tab may take to offer its root before a console window is created instead.
const OFFER_TIMEOUT: Duration = Duration::from_secs(8);
// How long the host may take to start the root and leave the console once the surface
// is bound.
const START_TIMEOUT: Duration = Duration::from_secs(5);
// How long the host waits for the started root to attach to the tab's console. Shorter
// than `START_TIMEOUT`, because the host confirms the start only afterwards.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(3);
// How long the host waits for Windows Terminal to take the console's hidden window.
const WINDOW_TIMEOUT: Duration = Duration::from_millis(300);
// How long a close keeps trying to read the record of the tab host.
const HOST_RECORD_READ_WAIT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostRequest {
    schema: u32,
    attempt: String,
    executable: String,
    command_line: String,
    deadline_unix_ms: u128,
    accepted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HostOffer {
    schema: u32,
    attempt: String,
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    identity: Option<WindowsProcessIdentity>,
    // The terminal window that hosts the tab, or 0 when the console reports none.
    #[serde(default)]
    window: u64,
    // Written once the root runs and the host has left the console.
    #[serde(default)]
    started: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HostProcess {
    schema: u32,
    pid: u32,
    identity: WindowsProcessIdentity,
}

/// The tab host that recorded itself in this session directory. `None` when the session
/// never had one. A record that is there and cannot be read is an error: a close that
/// went on without it could end the host with a failure code and leave its tab open.
pub(super) fn recorded_host(directory: &Path) -> Result<Option<(u32, WindowsProcessIdentity)>> {
    let path = directory.join(HOST_PROCESS_FILE);
    // The record is written once, by a rename, before the root exists. Only another
    // handle on the file can keep it from being read, and not for long.
    let deadline = Instant::now() + HOST_RECORD_READ_WAIT;
    loop {
        let read = read_record::<HostProcess>(&path).and_then(|record| match record {
            Some(host) if host.schema != 1 => {
                bail!("unsupported tab host record schema {}", host.schema)
            }
            host => Ok(host.map(|host| (host.pid, host.identity))),
        });
        match read {
            Ok(host) => return Ok(host),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| {
                    format!(
                        "the record of the Windows Terminal tab host cannot be read ({}); nothing was ended. Close the tab by hand if it is still open, remove that file, and close the session again",
                        path.display()
                    )
                });
            }
            Err(_) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Called when the launch gives the tab up for a console window. A record that an
/// abandoned host left is kept when it can be read: a close then ends that host too if
/// it is still there. One that cannot be read is removed, and when it cannot be removed
/// either the launch stops here, before a surface exists that no close could end.
pub(super) fn forget_unreadable_host(directory: &Path) -> Result<()> {
    let path = directory.join(HOST_PROCESS_FILE);
    if read_record::<HostProcess>(&path).is_ok_and(|host| host.is_none_or(|host| host.schema == 1))
    {
        return Ok(());
    }
    super::super::super::remove_file_if_present(&path).with_context(|| {
        format!(
            "the unreadable record of an abandoned Windows Terminal tab host could not be removed ({})",
            path.display()
        )
    })
}

// Required before a root is offered: without it a close cannot tell the host from the
// session's processes.
fn record_host(directory: &Path) -> Result<()> {
    let pid = std::process::id();
    write_record(
        &directory.join(HOST_PROCESS_FILE),
        &HostProcess {
            schema: 1,
            pid,
            identity: query_process_identity(pid)?,
        },
    )
    .context("the tab host could not record itself")
}

pub(super) struct TabSurface {
    directory: PathBuf,
    request: HostRequest,
    pid: u32,
    identity: WindowsProcessIdentity,
    window: Option<isize>,
    // Whether the offer was accepted, after which the host may start the root.
    accepted: Cell<bool>,
    // Whether this launch took the decision on the root, once it has tried to.
    gave_up: Cell<Option<bool>>,
}

impl TabSurface {
    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    pub(super) fn identity(&self) -> &WindowsProcessIdentity {
        &self.identity
    }

    pub(super) fn host_window(&self) -> Option<isize> {
        self.window
    }

    // Tells the host that the surface is bound and waits until it has started the root.
    //
    // A host that does not confirm the start in time has either not decided to start the
    // root, and then this launch decides that it never will, or it has decided, and the
    // root runs or is about to. In the second case the surface counts as started: from
    // there the launch is judged like every launch, by the provider start that the
    // wrapper records, and a launch that fails there keeps its surface bound.
    pub(super) fn start(&self, startup_deadline: Instant) -> Result<()> {
        if Instant::now() >= startup_deadline {
            bail!("Windows Terminal tab startup timed out before the managed console started");
        }
        let mut request = self.request.clone();
        request.accepted = true;
        // Set first: a write that fails may still have reached the host.
        self.accepted.set(true);
        write_record(&self.directory.join(REQUEST_FILE), &request)?;
        let deadline = startup_deadline.min(Instant::now() + START_TIMEOUT);
        loop {
            if let Some(offer) = read_offer(&self.directory, &self.request.attempt)? {
                if let Some(error) = offer.error {
                    bail!(
                        "the Windows Terminal tab host could not start the managed console: {error}"
                    );
                }
                if offer.started {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                if self.gives_up_first() {
                    bail!(
                        "the Windows Terminal tab host did not start the managed console in time"
                    );
                }
                return Ok(());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    // Takes the decision on the root for this launch, once. `false` when the host took
    // it first, or when it cannot be recorded here and the host may still take it.
    fn gives_up_first(&self) -> bool {
        if let Some(decided) = self.gave_up.get() {
            return decided;
        }
        let decided = decide(&self.directory).unwrap_or(false);
        self.gave_up.set(Some(decided));
        decided
    }

    // Gives the tab up. Whether the root is started is decided once, by whoever creates
    // the decision file first. When this launch decides, the host can no longer start
    // the root, which is still suspended, and ending it is all there is to do.
    //
    // When the host has decided, the root runs or is about to, and it starts the
    // session's wrapper and provider. Nothing is ended then: a console that was started
    // is ended as a whole by `close-session`, which finds it by its root, and the launch
    // that is being failed keeps the wrapper from starting a provider. The exception is
    // a host that says it could not start the root: it has ended the root itself.
    pub(super) fn cleanup(&self, deadline: Instant) -> Result<()> {
        // The host stops at its next read, unless it has already seen the acceptance.
        let _ = super::super::super::remove_file_if_present(&self.directory.join(REQUEST_FILE));
        // A host that never saw an acceptance never decides.
        if self.accepted.get() && !self.gives_up_first() {
            let refused = read_offer(&self.directory, &self.request.attempt)
                .ok()
                .flatten()
                .is_some_and(|offer| offer.error.is_some());
            if !refused {
                bail!(
                    "the tab host started the managed console; it stays bound to the session, which close-session ends"
                );
            }
        }
        terminate_by_identity(self.pid, &self.identity, deadline)?;
        let _ = super::super::super::remove_file_if_present(&self.directory.join(OFFER_FILE));
        Ok(())
    }

    // The handshake is over once the root runs; the host reads none of its files again.
    pub(super) fn finish(&self) {
        cancel(&self.directory);
    }
}

/// Asks Windows Terminal for a tab and waits for its host to offer the suspended root.
/// Every error is a reason why no tab can be had; the request is withdrawn in that case
/// and the caller creates a console window instead.
pub(super) fn offer(
    powershell: &Path,
    command_line: &str,
    directory: &Path,
    window: WindowsTabWindow,
    startup_deadline: Instant,
    guard: Option<&ForegroundGuard>,
) -> Result<TabSurface> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let terminal = locate_windows_terminal(&path)
        .context("Windows Terminal (wt.exe) was not found on an absolute PATH entry")?;
    if unsafe { GetShellWindow() }.is_null() {
        bail!("this desktop has no shell window");
    }
    let bridge = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let terminal_command = terminal_command_line(&terminal, &bridge, directory, window)?;
    let budget = startup_deadline.saturating_duration_since(Instant::now());
    let offer_deadline = Instant::now() + OFFER_TIMEOUT.min(budget / 2);
    let request = HostRequest {
        schema: 1,
        attempt: new_attempt(),
        executable: powershell
            .to_str()
            .context("PowerShell 7 executable path is not UTF-8")?
            .to_owned(),
        command_line: command_line.to_owned(),
        deadline_unix_ms: super::super::super::unix_ms() + budget.as_millis(),
        accepted: false,
    };
    cancel(directory);
    write_record(&directory.join(REQUEST_FILE), &request)?;
    let offered = (|| {
        let environment = environment_block(Some(&request.attempt));
        CreatedProcess::create(
            &terminal,
            &terminal_command,
            0,
            &console_startup_info(),
            Some(&environment),
        )
        .context("failed to start Windows Terminal (wt.exe)")?;
        loop {
            if let Some(offer) = read_offer(directory, &request.attempt)? {
                let window = isize::try_from(offer.window)
                    .ok()
                    .filter(|window| *window != 0);
                // The host window is known now, so the keyboard is given back before the
                // surface is bound and the host discards what was typed in between. A
                // tab that refuses has brought its window to the front all the same.
                if let (Some(guard), Some(window)) = (guard, window) {
                    guard.host_is(window);
                }
                if let Some(error) = offer.error {
                    bail!("the tab host refused: {error}");
                }
                let identity = offer
                    .identity
                    .context("the tab host offered no process identity")?;
                if let WindowsProcessIdentityCheck::Mismatch(reason) =
                    check_process_identity(offer.pid, &identity)
                        .context("the offered managed console root could not be inspected")?
                {
                    bail!("the offered managed console root is gone: {reason}");
                }
                return Ok((offer.pid, identity, window));
            }
            if Instant::now() >= offer_deadline {
                bail!("no tab host offered a managed console root in time");
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    })();
    match offered {
        Ok((pid, identity, window)) => Ok(TabSurface {
            directory: directory.to_owned(),
            request,
            pid,
            identity,
            window,
            accepted: Cell::new(false),
            gave_up: Cell::new(None),
        }),
        Err(error) => {
            // The offer was never accepted, so no host starts a root for it: one that
            // arrives or notices later finds no request and ends the root it made, and
            // the job that holds the root ends it if that host is gone.
            cancel(directory);
            Err(error)
        }
    }
}

/// The tab's root process. Never fails the tab: a refusal is written to the offer so that
/// the launcher creates a console window at once, and the tab closes.
pub(in crate::native::terminal) fn run_host(directory: &Path) -> Result<()> {
    host(
        directory,
        std::env::var(ATTEMPT_ENV).ok().as_deref(),
        &TabConsole,
    );
    Ok(())
}

// What the host does to the console of its tab, apart from creating the root in it.
trait HostConsole {
    // The terminal window that hosts the tab, or 0 when the console reports none.
    fn window(&self) -> u64;
    fn discard_pending_input(&self);
    fn leave_once_attached(&self, root: &CreatedProcess);
}

struct TabConsole;

impl HostConsole for TabConsole {
    // The console of a tab has a hidden window of its own, owned by the terminal window
    // that hosts the tab. Windows Terminal takes it when it creates the tab; the host
    // waits a moment in case it runs before that.
    fn window(&self) -> u64 {
        let deadline = Instant::now() + WINDOW_TIMEOUT;
        loop {
            if let Some(window) = attached_console_host_window() {
                return window as usize as u64;
            }
            if Instant::now() >= deadline {
                return 0;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn discard_pending_input(&self) {
        if let Ok(input) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
        {
            unsafe { FlushConsoleInputBuffer(input.as_raw_handle()) };
        }
    }

    // The session's console processes are ended with a failure code when it is closed,
    // and Windows Terminal keeps a tab whose root failed. The host therefore leaves the
    // console once the root has attached to it: it is then not one of the session's
    // console processes, ends on its own when the root does, and the tab closes. It stays
    // attached when the root never attaches, because leaving an empty console would close
    // the tab under the root.
    fn leave_once_attached(&self, root: &CreatedProcess) {
        let deadline = Instant::now() + ATTACH_TIMEOUT;
        loop {
            if unsafe { WaitForSingleObject(root.process.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
                return;
            }
            if console_process_ids().contains(&root.pid) {
                unsafe { FreeConsole() };
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

fn host(directory: &Path, inherited_attempt: Option<&str>, console: &dyn HostConsole) {
    // No request: the attempt was cancelled before the tab existed.
    let Ok(Some(request)) = read_request(directory) else {
        return;
    };
    let mut offer = HostOffer {
        schema: 1,
        attempt: request.attempt.clone(),
        ..HostOffer::default()
    };
    match host_root(directory, &request, inherited_attempt, &mut offer, console) {
        // The hold is kept until the root has ended: one that could not be released
        // would end the root as soon as it is dropped.
        Ok(Some((root, _hold))) => {
            // The root runs, and nothing from here on may end it. The confirmation
            // follows the host's leaving the console, so that a close that comes after
            // it does not find the host among the console's processes.
            console.leave_once_attached(&root);
            offer.started = true;
            let _ = write_record(&directory.join(OFFER_FILE), &offer);
            unsafe { WaitForSingleObject(root.process.as_raw_handle(), INFINITE) };
        }
        // Cancelled: the launcher has moved on and expects no offer.
        Ok(None) => {
            let _ = super::super::super::remove_file_if_present(&directory.join(OFFER_FILE));
        }
        // No error comes after the root was started.
        Err(error) => {
            offer.error = Some(format!("{error:#}"));
            let _ = write_record(&directory.join(OFFER_FILE), &offer);
        }
    }
}

// The launcher and the host decide once, together, whether the root is started: the side
// that creates this file has decided, and the other side finds it there. The host creates
// it to start the root, the launcher to give the tab up, so the root is never started
// after the launcher has given it up, and never ended alone after the host has started
// it. Returns whether the caller decided.
fn decide(directory: &Path) -> Result<bool> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(DECISION_FILE))
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).context("failed to record the decision on the tab's root"),
    }
}

// A job that ends the root when the host ends, so that a host that dies before the root
// runs leaves no process behind. The job exists before the root, and the root is created
// inside it, so there is no moment at which the root is outside it. It is released only
// after the root was started: a host that dies in between takes the root with it.
struct RootHold {
    job: OwnedHandle,
}

impl RootHold {
    fn new() -> Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create the job that holds the suspended root");
        }
        let hold = Self {
            job: unsafe { OwnedHandle::from_raw_handle(job) },
        };
        hold.limit(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        Ok(hold)
    }

    // The root stays a member of the job, which no longer ends it, and what the root
    // starts from now on is not made a member.
    fn release(&self) -> Result<()> {
        self.limit(JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
    }

    fn limit(&self, flags: u32) -> Result<()> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = flags;
        if unsafe {
            SetInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to configure the job that holds the suspended root");
        }
        Ok(())
    }
}

// Creates the root suspended, offers it, and starts it once the launcher has accepted the
// offer. `None` when the attempt was cancelled or ran out of time. The root is ended
// before it ran in that case and in every error.
fn host_root(
    directory: &Path,
    request: &HostRequest,
    inherited_attempt: Option<&str>,
    offer: &mut HostOffer,
    console: &dyn HostConsole,
) -> Result<Option<(CreatedProcess, RootHold)>> {
    if inherited_attempt != Some(request.attempt.as_str()) {
        bail!("the tab did not inherit the environment of the launch that asked for it");
    }
    let expired = || super::super::super::unix_ms() >= request.deadline_unix_ms;
    if expired() {
        return Ok(None);
    }
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    record_host(directory)?;
    let hold = RootHold::new()?;
    let root = CreatedProcess::create_in_job(
        Path::new(&request.executable),
        &request.command_line,
        CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED,
        &startup,
        Some(&environment_block(None)),
        hold.job.as_raw_handle(),
    )
    .context("failed to create the managed PowerShell root in the tab")?;
    let started = (|| -> Result<bool> {
        // First, so that a refusal names the window too: the tab brought it to the front
        // whether or not the session ends up in it.
        offer.window = console.window();
        offer.pid = root.pid;
        offer.identity = Some(query_process_identity_from_handle(
            root.process.as_raw_handle(),
        )?);
        write_record(&directory.join(OFFER_FILE), offer)?;
        loop {
            match read_request(directory)? {
                Some(current) if current.attempt == request.attempt => {
                    if current.accepted {
                        break;
                    }
                }
                _ => return Ok(false),
            }
            if expired() {
                return Ok(false);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // The launcher may have given the tab up since it accepted the offer.
        if !decide(directory)? {
            return Ok(false);
        }
        // A new tab has the keyboard until the launcher has given it back, and a key
        // typed in that time was meant for another window. It must not reach the session:
        // an Enter would answer the first dialog the provider shows.
        console.discard_pending_input();
        root.resume()
            .context("failed to start the managed PowerShell root")?;
        Ok(true)
    })();
    if matches!(started, Ok(true)) {
        // A hold that cannot be released stays in force: the root then ends with the
        // host, which lives as long as the root does.
        let _ = hold.release();
        return Ok(Some((root, hold)));
    }
    // Not started: ending the root is ending everything.
    let ended = terminate_process_until(
        root.process.as_raw_handle(),
        Instant::now() + Duration::from_secs(2),
    );
    match (started, ended) {
        (Err(error), _) => Err(error),
        (_, Err(error)) => {
            Err(error.context("the cancelled managed PowerShell root is still running"))
        }
        _ => Ok(None),
    }
}

fn console_process_ids() -> Vec<u32> {
    let mut processes = vec![0u32; 64];
    loop {
        let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) }
            as usize;
        if count <= processes.len() {
            processes.truncate(count);
            return processes;
        }
        processes.resize(count, 0);
    }
}

// `wt.exe` is an execution alias when Windows Terminal is installed as a package: a
// reparse point that only `CreateProcessW` resolves, so it is located without following it.
fn locate_windows_terminal(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("wt.exe"))
        .find(|candidate| {
            std::fs::symlink_metadata(candidate).is_ok_and(|metadata| !metadata.is_dir())
        })
}

// Windows Terminal reads `;` as the end of a command and expands `%NAME%` in the command
// line it runs, and it quotes an argument with a space without escaping anything in it.
// A value it could change is refused, and the launch uses a console window instead.
fn tab_argument(value: &Path, field: &str) -> Result<String> {
    let text = value
        .to_str()
        .with_context(|| format!("{field} is not UTF-8"))?;
    if let Some(character) = text
        .chars()
        .find(|character| matches!(character, ';' | '%' | '"') || character.is_control())
    {
        bail!("{field} contains {character:?}, which Windows Terminal does not pass on unchanged");
    }
    if text.is_empty() || text.starts_with('-') || text.ends_with('\\') {
        bail!("{field} cannot be passed to Windows Terminal unchanged");
    }
    Ok(format!("\"{text}\""))
}

fn terminal_command_line(
    terminal: &Path,
    bridge: &Path,
    directory: &Path,
    window: WindowsTabWindow,
) -> Result<String> {
    // `0` is Windows Terminal's name for its most recently used window. Either target
    // makes Windows Terminal create the window when there is none.
    let target = match window {
        WindowsTabWindow::Dedicated => WINDOW_NAME,
        WindowsTabWindow::Current => "0",
    };
    Ok(format!(
        "{} -w {target} new-tab {} {HOST_COMMAND} {}",
        tab_argument(terminal, "the Windows Terminal path")?,
        tab_argument(bridge, "the Agent Bridge executable path")?,
        tab_argument(directory, "the session directory")?,
    ))
}

// The environment of this process, as a block for `CreateProcessW`, with the attempt
// marker set to `attempt` or absent. The names are sorted without regard to case, as the
// system expects of a block.
fn environment_block(attempt: Option<&str>) -> Vec<u16> {
    let mut variables = std::env::vars_os()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(ATTEMPT_ENV))
        .map(|(name, value)| (name.to_string_lossy().to_uppercase(), (name, value)))
        .collect::<BTreeMap<_, _>>();
    if let Some(attempt) = attempt {
        variables.insert(
            ATTEMPT_ENV.to_owned(),
            (OsString::from(ATTEMPT_ENV), OsString::from(attempt)),
        );
    }
    let mut block = Vec::new();
    for (name, value) in variables.into_values() {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn new_attempt() -> String {
    format!("{}-{}", std::process::id(), super::super::super::unix_ms())
}

// The session directory is private, and neither record has to survive a crash: the
// handshake is over within seconds. The rename keeps a reader from seeing half a record.
fn write_record<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("handshake record has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".console-host-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    std::io::Write::write_all(&mut temporary, &serde_json::to_vec(value)?)?;
    // The other side polls this record, and Windows refuses to replace a file that is open.
    super::super::super::persist_record(temporary, path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn read_record<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let Some(text) = super::super::super::read_regular_text_if_present(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn read_request(directory: &Path) -> Result<Option<HostRequest>> {
    Ok(read_record::<HostRequest>(&directory.join(REQUEST_FILE))?
        .filter(|request| request.schema == 1 && !request.attempt.is_empty()))
}

// An offer of another attempt is a leftover and is not this launch's.
fn read_offer(directory: &Path, attempt: &str) -> Result<Option<HostOffer>> {
    Ok(read_record::<HostOffer>(&directory.join(OFFER_FILE))?
        .filter(|offer| offer.schema == 1 && offer.attempt == attempt))
}

fn cancel(directory: &Path) {
    for file in [REQUEST_FILE, OFFER_FILE, DECISION_FILE] {
        let _ = super::super::super::remove_file_if_present(&directory.join(file));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;

    fn request(directory: &Path, attempt: &str, marker: &Path) -> HostRequest {
        let shell = std::env::var_os("ComSpec").unwrap();
        let request = HostRequest {
            schema: 1,
            attempt: attempt.to_owned(),
            executable: shell.to_str().unwrap().to_owned(),
            command_line: format!(
                "\"{}\" /d /c echo started> \"{}\"",
                shell.to_str().unwrap(),
                marker.display()
            ),
            deadline_unix_ms: super::super::super::super::unix_ms() + 20_000,
            accepted: false,
        };
        write_record(&directory.join(REQUEST_FILE), &request).unwrap();
        request
    }

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    // Records what the host asks of its console, together with what the root and the
    // offer said at that moment.
    struct RecordingConsole {
        directory: PathBuf,
        attempt: String,
        marker: PathBuf,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingConsole {
        fn new(directory: &Path, attempt: &str, marker: &Path) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                directory: directory.to_owned(),
                attempt: attempt.to_owned(),
                marker: marker.to_owned(),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn offer(&self) -> HostOffer {
            read_offer(&self.directory, &self.attempt)
                .unwrap()
                .expect("the host has offered its root")
        }
    }

    impl HostConsole for RecordingConsole {
        fn window(&self) -> u64 {
            self.calls.lock().unwrap().push("window".to_owned());
            0x1234
        }

        fn discard_pending_input(&self) {
            self.calls.lock().unwrap().push(format!(
                "discard input; root ran: {}, start decided: {}",
                self.marker.exists(),
                self.directory.join(DECISION_FILE).exists()
            ));
        }

        fn leave_once_attached(&self, _root: &CreatedProcess) {
            self.calls.lock().unwrap().push(format!(
                "leave the console; start confirmed: {}",
                self.offer().started
            ));
        }
    }

    #[test]
    fn the_root_is_offered_suspended_and_started_only_after_the_surface_is_bound() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        let mut accepted = request(directory.path(), "attempt-1", &marker);
        let console = RecordingConsole::new(directory.path(), "attempt-1", &marker);
        let path = directory.path().to_owned();
        let worker_console = console.clone();
        let worker =
            std::thread::spawn(move || host(&path, Some("attempt-1"), worker_console.as_ref()));

        let offer = wait_for(|| read_offer(directory.path(), "attempt-1").unwrap());
        let identity = offer.identity.clone().unwrap();
        assert_eq!(
            check_process_identity(offer.pid, &identity).unwrap(),
            WindowsProcessIdentityCheck::Matches
        );
        assert_eq!(offer.window, 0x1234);
        assert!(!offer.started);
        // Suspended: the root has not run its command, and nothing is decided.
        std::thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists());
        assert!(!directory.path().join(DECISION_FILE).exists());
        assert_eq!(console.calls(), ["window"]);
        assert!(
            read_offer(directory.path(), "another-attempt")
                .unwrap()
                .is_none()
        );

        accepted.accepted = true;
        write_record(&directory.path().join(REQUEST_FILE), &accepted).unwrap();
        wait_for(|| {
            read_offer(directory.path(), "attempt-1")
                .unwrap()
                .filter(|offer| offer.started)
        });
        worker.join().unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "started");
        // The host decides before the root runs and discards what was typed into the new
        // tab; it confirms the start only after it has left the console.
        assert_eq!(
            console.calls(),
            [
                "window",
                "discard input; root ran: false, start decided: true",
                "leave the console; start confirmed: false"
            ]
        );
        // The launcher that gives up now finds the decision taken.
        assert!(!decide(directory.path()).unwrap());
        // The host recorded itself for a close that finds it still in the console. In
        // this test it runs on a thread of the test process.
        let (host_pid, host_identity) = recorded_host(directory.path()).unwrap().unwrap();
        assert_eq!(host_pid, std::process::id());
        assert_eq!(host_identity, query_process_identity(host_pid).unwrap());
    }

    // A close knows the host only by its record. A host that cannot write it offers no
    // root, and the launch opens a console window instead.
    #[test]
    fn a_host_that_cannot_record_itself_offers_no_root() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        request(directory.path(), "attempt-12", &marker);
        // A directory where the record belongs keeps it from being written.
        fs::create_dir(directory.path().join(HOST_PROCESS_FILE)).unwrap();
        let console = RecordingConsole::new(directory.path(), "attempt-12", &marker);

        host(directory.path(), Some("attempt-12"), console.as_ref());

        let offer = console.offer();
        assert_eq!(offer.pid, 0);
        assert!(
            offer
                .error
                .unwrap()
                .contains("the tab host could not record itself")
        );
        assert!(console.calls().is_empty());
        assert!(!marker.exists());
    }

    // The launch that falls back to a console window leaves no record behind that a
    // close could not read.
    #[test]
    fn a_fallback_keeps_a_readable_host_record_and_removes_or_refuses_an_unreadable_one() {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join(HOST_PROCESS_FILE);
        // Nothing there: nothing to do.
        forget_unreadable_host(directory.path()).unwrap();

        // A host that recorded itself before the tab was given up: kept.
        record_host(directory.path()).unwrap();
        forget_unreadable_host(directory.path()).unwrap();
        assert!(recorded_host(directory.path()).unwrap().is_some());

        // A record that cannot be read is removed.
        fs::write(&record, b"{").unwrap();
        forget_unreadable_host(directory.path()).unwrap();
        assert!(!record.exists());
        assert!(recorded_host(directory.path()).unwrap().is_none());

        // What cannot be removed either stops the launch.
        fs::create_dir(&record).unwrap();
        let error = forget_unreadable_host(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("could not be removed"));
    }

    #[test]
    fn a_host_record_that_cannot_be_read_is_an_error_and_a_missing_one_is_no_host() {
        let directory = tempfile::tempdir().unwrap();
        assert!(recorded_host(directory.path()).unwrap().is_none());

        for unreadable in [
            &b"{"[..],
            br#"{"schema":2,"pid":1,"identity":{"creation_time":1,"executable_path":"x"}}"#,
        ] {
            fs::write(directory.path().join(HOST_PROCESS_FILE), unreadable).unwrap();
            let error = recorded_host(directory.path()).unwrap_err();
            assert!(
                format!("{error:#}")
                    .contains("the record of the Windows Terminal tab host cannot be read")
            );
        }
    }

    // The launcher accepted the offer and gave the tab up before the host came to
    // start the root. The host finds the decision and starts nothing.
    #[test]
    fn a_host_that_finds_the_tab_given_up_does_not_start_the_root() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        let mut accepted = request(directory.path(), "attempt-11", &marker);
        accepted.accepted = true;
        write_record(&directory.path().join(REQUEST_FILE), &accepted).unwrap();
        assert!(decide(directory.path()).unwrap());
        let console = RecordingConsole::new(directory.path(), "attempt-11", &marker);

        host(directory.path(), Some("attempt-11"), console.as_ref());

        assert!(!marker.exists());
        assert!(!directory.path().join(OFFER_FILE).exists());
        assert_eq!(console.calls(), ["window"]);
    }

    #[test]
    fn a_root_that_cannot_be_created_is_refused_without_any_process() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        let mut missing = request(directory.path(), "attempt-0", &marker);
        missing.executable = directory
            .path()
            .join("missing.exe")
            .to_str()
            .unwrap()
            .to_owned();
        write_record(&directory.path().join(REQUEST_FILE), &missing).unwrap();
        let console = RecordingConsole::new(directory.path(), "attempt-0", &marker);

        host(directory.path(), Some("attempt-0"), console.as_ref());

        let offer = console.offer();
        assert_eq!(offer.pid, 0);
        assert!(!offer.started);
        assert!(
            offer
                .error
                .unwrap()
                .contains("failed to create the managed PowerShell root")
        );
        assert!(console.calls().is_empty());
        assert!(!directory.path().join(DECISION_FILE).exists());
    }

    fn startup() -> STARTUPINFOW {
        STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        }
    }

    fn suspended_root() -> (CreatedProcess, WindowsProcessIdentity) {
        let root = CreatedProcess::create(
            Path::new(&std::env::var_os("ComSpec").unwrap()),
            "cmd.exe /d /c exit 0",
            CREATE_SUSPENDED,
            &startup(),
            None,
        )
        .unwrap();
        let identity = query_process_identity_from_handle(root.process.as_raw_handle()).unwrap();
        (root, identity)
    }

    // A suspended root as the host creates it: inside the job that holds it.
    fn held_root() -> (RootHold, CreatedProcess) {
        let hold = RootHold::new().unwrap();
        let root = CreatedProcess::create_in_job(
            Path::new(&std::env::var_os("ComSpec").unwrap()),
            "cmd.exe /d /c exit 0",
            CREATE_SUSPENDED,
            &startup(),
            None,
            hold.job.as_raw_handle(),
        )
        .unwrap();
        (hold, root)
    }

    fn ended(root: &CreatedProcess) -> bool {
        unsafe { WaitForSingleObject(root.process.as_raw_handle(), 2_000) == WAIT_OBJECT_0 }
    }

    // A host that dies holds nothing any more: the job is the only thing that ends a
    // root nobody has been told about yet. The root is in it from its creation.
    #[test]
    fn a_host_that_ends_before_the_root_runs_takes_the_root_with_it() {
        let (hold, root) = held_root();
        let mut member = 0;
        assert_ne!(
            unsafe {
                IsProcessInJob(
                    root.process.as_raw_handle(),
                    hold.job.as_raw_handle(),
                    &mut member,
                )
            },
            0
        );
        assert_ne!(member, 0, "the root is created inside the job");
        assert!(agent_bridge::process_is_alive(root.pid));
        drop(hold);
        assert!(ended(&root), "the held root ends with its holder");

        // Released, the root no longer ends with the host.
        let (hold, root) = held_root();
        hold.release().unwrap();
        drop(hold);
        assert!(
            unsafe { WaitForSingleObject(root.process.as_raw_handle(), 300) } != WAIT_OBJECT_0,
            "a released root outlives the hold"
        );
        let identity = query_process_identity_from_handle(root.process.as_raw_handle()).unwrap();
        terminate_by_identity(root.pid, &identity, deadline()).unwrap();
    }

    fn surface(directory: &Path, attempt: &str) -> TabSurface {
        let (root, identity) = suspended_root();
        write_record(
            &directory.join(REQUEST_FILE),
            &HostRequest {
                schema: 1,
                attempt: attempt.to_owned(),
                executable: String::new(),
                command_line: String::new(),
                deadline_unix_ms: u128::MAX,
                accepted: false,
            },
        )
        .unwrap();
        write_record(
            &directory.join(OFFER_FILE),
            &HostOffer {
                schema: 1,
                attempt: attempt.to_owned(),
                pid: root.pid,
                ..HostOffer::default()
            },
        )
        .unwrap();
        TabSurface {
            directory: directory.to_owned(),
            request: read_request(directory).unwrap().unwrap(),
            pid: root.pid,
            identity,
            window: None,
            accepted: Cell::new(false),
            gave_up: Cell::new(None),
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    // A startup deadline that a host which does nothing runs into at once.
    fn short_deadline() -> Instant {
        Instant::now() + Duration::from_millis(120)
    }

    fn files(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    // A host that never saw an acceptance cannot decide to start the root, so nothing is
    // decided here either.
    #[test]
    fn giving_up_an_offer_that_was_not_accepted_ends_the_suspended_root_only() {
        let directory = tempfile::tempdir().unwrap();
        let tab = surface(directory.path(), "attempt-6");

        tab.cleanup(deadline()).unwrap();

        assert!(!agent_bridge::process_is_alive(tab.pid));
        assert!(files(directory.path()).is_empty());
    }

    // The host does not confirm the start and has not decided to start the root. The
    // launch decides that it never will, so the root is still suspended and ending it
    // is complete. The decision stays for a host that looks later.
    #[test]
    fn a_host_that_does_not_start_the_root_in_time_never_starts_it() {
        let directory = tempfile::tempdir().unwrap();
        let tab = surface(directory.path(), "attempt-7");

        let error = tab.start(short_deadline()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not start the managed console in time")
        );
        assert!(
            read_request(directory.path()).unwrap().unwrap().accepted,
            "the host was told that the surface is bound"
        );
        assert!(!decide(directory.path()).unwrap());

        tab.cleanup(deadline()).unwrap();
        assert!(!agent_bridge::process_is_alive(tab.pid));
        assert_eq!(files(directory.path()), [DECISION_FILE]);
    }

    // The host decided to start the root and has not confirmed it yet: the root runs or
    // is about to. The surface counts as started, and the launch goes on to wait for the
    // provider like every launch. Nothing is ended.
    #[test]
    fn a_host_that_decided_but_has_not_confirmed_leaves_the_surface_started() {
        let directory = tempfile::tempdir().unwrap();
        let tab = surface(directory.path(), "attempt-8");
        assert!(decide(directory.path()).unwrap());

        tab.start(short_deadline()).unwrap();

        assert!(agent_bridge::process_is_alive(tab.pid));
        terminate_by_identity(tab.pid, &tab.identity, deadline()).unwrap();
    }

    // The root is the process by which a later close finds the console. Ended alone, it
    // would leave the wrapper and the provider it started where nothing finds them.
    #[test]
    fn giving_up_after_the_host_decided_ends_nothing_and_keeps_the_binding() {
        let directory = tempfile::tempdir().unwrap();
        let tab = surface(directory.path(), "attempt-9");
        tab.accepted.set(true);
        assert!(decide(directory.path()).unwrap());

        let error = tab.cleanup(deadline()).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("stays bound to the session"));
        assert!(message.contains("close-session"));
        assert!(agent_bridge::process_is_alive(tab.pid));
        // The request is withdrawn all the same.
        assert!(!directory.path().join(REQUEST_FILE).exists());
        terminate_by_identity(tab.pid, &tab.identity, deadline()).unwrap();
    }

    // A host that decided and then could not start the root has ended it and says so.
    #[test]
    fn a_host_that_reports_a_failed_start_leaves_nothing_to_keep() {
        let directory = tempfile::tempdir().unwrap();
        let tab = surface(directory.path(), "attempt-10");
        assert!(decide(directory.path()).unwrap());
        let mut offer = read_offer(directory.path(), "attempt-10").unwrap().unwrap();
        offer.error = Some("failed to start the managed PowerShell root".to_owned());
        write_record(&directory.path().join(OFFER_FILE), &offer).unwrap();

        let error = tab.start(deadline()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not start the managed console")
        );

        tab.cleanup(deadline()).unwrap();
        assert!(!agent_bridge::process_is_alive(tab.pid));
        assert_eq!(files(directory.path()), [DECISION_FILE]);
    }

    #[test]
    fn the_decision_on_the_root_is_taken_once() {
        let directory = tempfile::tempdir().unwrap();
        assert!(decide(directory.path()).unwrap());
        assert!(!decide(directory.path()).unwrap());
        assert!(!decide(directory.path()).unwrap());
        // A directory that is gone decides nothing; the caller treats that as unknown.
        assert!(decide(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn a_cancelled_attempt_ends_the_suspended_root_and_leaves_no_offer() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        request(directory.path(), "attempt-2", &marker);
        let console = RecordingConsole::new(directory.path(), "attempt-2", &marker);
        let path = directory.path().to_owned();
        let worker_console = console.clone();
        let worker =
            std::thread::spawn(move || host(&path, Some("attempt-2"), worker_console.as_ref()));

        let offer = wait_for(|| read_offer(directory.path(), "attempt-2").unwrap());
        fs::remove_file(directory.path().join(REQUEST_FILE)).unwrap();
        worker.join().unwrap();

        assert!(!agent_bridge::process_is_alive(offer.pid));
        assert!(!marker.exists());
        assert!(!directory.path().join(OFFER_FILE).exists());
        assert_eq!(console.calls(), ["window"]);
    }

    #[test]
    fn a_tab_without_the_launchers_environment_is_refused_before_any_root_exists() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        request(directory.path(), "attempt-3", &marker);
        let console = RecordingConsole::new(directory.path(), "attempt-3", &marker);

        for inherited in [None, Some("another-attempt")] {
            host(directory.path(), inherited, console.as_ref());
            let offer = read_offer(directory.path(), "attempt-3").unwrap().unwrap();
            assert_eq!(offer.pid, 0);
            assert!(offer.identity.is_none());
            assert!(offer.error.unwrap().contains("did not inherit"));
        }
        assert!(!marker.exists());
        assert!(console.calls().is_empty());
    }

    #[test]
    fn a_host_without_a_request_does_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let console = RecordingConsole::new(
            directory.path(),
            "attempt-4",
            &directory.path().join("started.txt"),
        );
        host(directory.path(), Some("attempt-4"), console.as_ref());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(console.calls().is_empty());
    }

    #[test]
    fn an_expired_attempt_creates_no_root() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("started.txt");
        let mut expired = request(directory.path(), "attempt-5", &marker);
        expired.deadline_unix_ms = 1;
        write_record(&directory.path().join(REQUEST_FILE), &expired).unwrap();
        let console = RecordingConsole::new(directory.path(), "attempt-5", &marker);

        host(directory.path(), Some("attempt-5"), console.as_ref());
        assert!(!directory.path().join(OFFER_FILE).exists());
        assert!(!marker.exists());
        assert!(console.calls().is_empty());
    }

    #[test]
    fn the_offered_root_is_ended_by_its_identity_and_a_reused_pid_is_left_alone() {
        let (root, identity) = suspended_root();
        let other = WindowsProcessIdentity {
            creation_time: identity.creation_time.wrapping_add(1),
            executable_path: identity.executable_path.clone(),
        };
        let deadline = Instant::now() + Duration::from_secs(2);

        terminate_by_identity(root.pid, &other, deadline).unwrap();
        assert!(agent_bridge::process_is_alive(root.pid));
        terminate_by_identity(root.pid, &identity, deadline).unwrap();
        assert!(!agent_bridge::process_is_alive(root.pid));
        terminate_by_identity(root.pid, &identity, deadline).unwrap();
    }

    #[test]
    fn the_tab_goes_to_the_agent_bridge_window_and_runs_only_the_host() {
        let command_line = |window| {
            terminal_command_line(
                Path::new(r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\wt.exe"),
                Path::new(r"C:\Program Files\Agent Bridge\agent-bridge.exe"),
                Path::new(r"C:\Users\me\.agent-bridge\native-sessions\session-AbC123"),
                window,
            )
            .unwrap()
        };
        let command = command_line(WindowsTabWindow::default());
        assert_eq!(
            command,
            concat!(
                r#""C:\Users\me\AppData\Local\Microsoft\WindowsApps\wt.exe" -w agent-bridge new-tab "#,
                r#""C:\Program Files\Agent Bridge\agent-bridge.exe" native-console-host "#,
                r#""C:\Users\me\.agent-bridge\native-sessions\session-AbC123""#
            )
        );
        // Unless the user chose it, never the window they work in, and never a new window
        // on purpose.
        for target in ["-w 0", "-w last", "-w new", "-w -1"] {
            assert!(!command.contains(target));
        }

        // The user's choice of the most recently used window changes the target only.
        assert_eq!(
            command_line(WindowsTabWindow::Current),
            command.replace("-w agent-bridge ", "-w 0 ")
        );
    }

    #[test]
    fn a_path_that_windows_terminal_would_change_is_refused() {
        let terminal = Path::new(r"C:\wt.exe");
        let bridge = Path::new(r"C:\agent-bridge.exe");
        let command_line = |bridge: &Path, directory: &Path| {
            terminal_command_line(terminal, bridge, directory, WindowsTabWindow::default())
        };
        for directory in [
            r"C:\state;x\session-1",
            r"C:\state\%TEMP%\session-1",
            r#"C:\state\"quoted"\session-1"#,
            r"C:\state\session-1\",
            "C:\\state\tx\\session-1",
            "-state",
            "",
        ] {
            assert!(
                command_line(bridge, Path::new(directory)).is_err(),
                "{directory:?}"
            );
        }
        assert!(command_line(Path::new(r"C:\a;b\bridge.exe"), bridge).is_err());
        assert!(
            command_line(
                bridge,
                Path::new(r"C:\Users\한글 사용자\state's & (x)\session-1")
            )
            .is_ok()
        );
    }

    #[test]
    fn windows_terminal_is_located_only_on_absolute_path_entries() {
        let directory = tempfile::tempdir().unwrap();
        let relative = directory.path().join("relative");
        let trusted = directory.path().join("trusted");
        let folder = directory.path().join("folder");
        fs::create_dir_all(&relative).unwrap();
        fs::create_dir_all(&trusted).unwrap();
        fs::create_dir_all(folder.join("wt.exe")).unwrap();
        fs::write(relative.join("wt.exe"), b"planted").unwrap();
        fs::write(trusted.join("wt.exe"), b"trusted").unwrap();

        let path = std::env::join_paths([Path::new("relative"), &folder, &trusted]).unwrap();
        assert_eq!(locate_windows_terminal(&path), Some(trusted.join("wt.exe")));
        let missing = std::env::join_paths([Path::new("relative"), &folder]).unwrap();
        assert_eq!(locate_windows_terminal(&missing), None);
    }

    #[test]
    fn the_attempt_marker_reaches_only_windows_terminal() {
        fn names(block: &[u16]) -> Vec<String> {
            block
                .split(|unit| *unit == 0)
                .filter(|entry| !entry.is_empty())
                .map(String::from_utf16_lossy)
                .collect()
        }
        let marked = names(&environment_block(Some("attempt-6")));
        assert!(marked.contains(&format!("{ATTEMPT_ENV}=attempt-6")));
        let plain = names(&environment_block(None));
        assert!(!plain.iter().any(|entry| entry.starts_with(ATTEMPT_ENV)));
        // Everything else is passed on unchanged.
        assert_eq!(marked.len(), plain.len() + 1);
        assert!(
            plain
                .iter()
                .any(|entry| entry.to_uppercase().starts_with("PATH="))
        );
        assert!(environment_block(None).ends_with(&[0, 0]));
    }
}
