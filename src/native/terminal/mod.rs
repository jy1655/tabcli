pub(super) mod close_evidence;
pub(in crate::native) mod host;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) use close_evidence::for_kind as close_evidence;
use std::time::Duration;
pub(super) mod facts;
mod linux_birth;
pub(in crate::native) use linux_birth::LinuxBirth;

/// Provider-selected encoding of LF in a native Windows console text batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::native) enum ConsoleNewline {
    LineFeed,
    ShiftReturn,
}

pub(super) const fn facts(kind: TerminalKind) -> facts::SurfaceFacts {
    kind.registration().facts
}

pub(super) fn surface_outlives_owner(session: &TerminalSession) -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return facts(session.kind).outlives_owner;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = session;
        false
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(in crate::native::terminal) mod bound_host;
pub(in crate::native) mod close_authority;
#[cfg(target_os = "linux")]
pub(in crate::native) mod linux;
pub(in crate::native) mod ownership;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) mod posix;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) mod posix_ownership;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::native) mod process;
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn posix_installed() -> &'static dyn posix::Adapters {
    #[cfg(target_os = "macos")]
    return macos::INSTALLED;
    #[cfg(target_os = "linux")]
    return &linux::INSTALLED;
}

use std::{path::Path, str::FromStr, time::Instant};

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
use anyhow::bail;
use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod herdr;
#[cfg(target_os = "macos")]
pub(super) mod macos;
// Pure wire validation is exercised on every test platform.
#[cfg(any(target_os = "macos", test))]
#[path = "macos/shell_environment/wire.rs"]
mod shell_environment_wire;
// The capture's time budget is a pure rule, and doctor reads its limit on every target.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
#[path = "macos/shell_environment/budget.rs"]
pub(in crate::native) mod shell_environment_budget;
// Compiled into every test build so its refusal is tested on every CI target.
#[cfg(any(not(any(target_os = "macos", target_os = "windows")), test))]
#[cfg_attr(
    any(target_os = "macos", target_os = "linux", target_os = "windows"),
    allow(dead_code)
)]
mod refusing;
#[cfg(target_os = "windows")]
pub(in crate::native) mod windows;

// Platform-only pure decisions live beside their platform module and compile on every
// target, so that their tests run on every target; the platform modules remain gated.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[path = "macos/dialog.rs"]
mod macos_dialog;
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
#[path = "macos/selection.rs"]
mod macos_selection;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[path = "windows/budget.rs"]
pub(in crate::native) mod windows_budget;
#[cfg(any(windows, test))]
#[path = "windows/dispatch.rs"]
mod windows_dispatch;

/// The installed Windows lookup: the managed console and Herdr.
#[cfg(windows)]
fn windows_installed() -> &'static dyn windows_dispatch::Adapters {
    &windows::INSTALLED
}

/// A guarded dialog response was refused before any input was dispatched.
#[derive(Debug)]
pub(in crate::native) struct StaleDialogClaim;

impl std::fmt::Display for StaleDialogClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stale claim at guarded submission")
    }
}

impl std::error::Error for StaleDialogClaim {}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
use refusing as platform;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TerminalKind {
    Iterm2,
    AppleTerminal,
    Ghostty,
    Warp,
    #[serde(rename = "wezterm")]
    WezTerm,
    WindowsConsole,
    Tmux,
    /// A terminal of a running Herdr Core (`hc`), addressed through its local socket.
    Herdr,
}

impl TerminalKind {
    /// Every kind, in declaration order.
    pub(in crate::native) const ALL: [Self; 8] = [
        Self::Iterm2,
        Self::AppleTerminal,
        Self::Ghostty,
        Self::Warp,
        Self::WezTerm,
        Self::WindowsConsole,
        Self::Tmux,
        Self::Herdr,
    ];

    pub(super) const fn supported_on_this_platform(self) -> bool {
        match self {
            Self::Iterm2 | Self::AppleTerminal | Self::Ghostty | Self::Warp | Self::WezTerm => {
                cfg!(target_os = "macos")
            }
            Self::Herdr => cfg!(any(target_os = "macos", target_os = "linux", windows)),
            Self::WindowsConsole => cfg!(windows),
            Self::Tmux => cfg!(target_os = "linux"),
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Iterm2 => "iterm2",
            Self::AppleTerminal => "apple-terminal",
            Self::Ghostty => "ghostty",
            Self::Warp => "warp",
            Self::WezTerm => "wezterm",
            Self::WindowsConsole => "windows-console",
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
        }
    }

    pub(super) const fn display_name(self) -> &'static str {
        match self {
            Self::Iterm2 => "iTerm2",
            Self::AppleTerminal => "Terminal.app",
            Self::Ghostty => "Ghostty",
            Self::Warp => "Warp",
            Self::WezTerm => "WezTerm",
            Self::WindowsConsole => "Windows Console",
            Self::Tmux => "tmux",
            Self::Herdr => "Herdr",
        }
    }
}

impl FromStr for TerminalKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "ghostty" => Ok(Self::Ghostty),
            "warp" | "warpterminal" => Ok(Self::Warp),
            "wezterm" => Ok(Self::WezTerm),
            "windows-console" | "windows" | "console" => Ok(Self::WindowsConsole),
            "tmux" => Ok(Self::Tmux),
            "herdr" | "hc" | "herdr-core" => Ok(Self::Herdr),
            "iterm" | "iterm.app" | "iterm2" => Ok(Self::Iterm2),
            "apple-terminal" | "apple_terminal" | "default" | "terminal" | "terminal.app" => {
                Ok(Self::AppleTerminal)
            }
            _ => Err(format!(
                "unsupported terminal {value:?}; expected ghostty, herdr, iterm2, terminal, tmux, warp, wezterm, or windows-console"
            )),
        }
    }
}

mod record;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(in crate::native) use record::HerdrSurface;
#[cfg(target_os = "linux")]
pub(in crate::native) use record::LinuxHostIdentity;
pub(in crate::native) use record::TerminalSession;

// Only an adapter that has proven its exact surface may return this error. The
// shared launcher persists the handle; it grants no new adapter close authority.
#[derive(Debug)]
pub(super) struct RetainedLaunchSurface {
    pub(super) surface: TerminalSession,
    pub(super) unverified_cleanup: bool,
    message: String,
}

impl RetainedLaunchSurface {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(super) fn new(surface: TerminalSession, message: String) -> Self {
        Self {
            surface,
            message,
            unverified_cleanup: false,
        }
    }

    // Only the macOS adapters retain a surface whose cleanup is unverified.
    #[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
    pub(super) fn with_unverified_cleanup(mut self) -> Self {
        self.unverified_cleanup = true;
        self
    }
}

impl std::fmt::Display for RetainedLaunchSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RetainedLaunchSurface {}

// The WezTerm GUI incarnation that serves a session. A mux numbers its panes from 0
// (`static PANE_ID` in mux/src/pane.rs), so the pane id of the session is an identity only
// together with that process: its pid, its birth, and the socket it serves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct WezTermMux {
    pub(super) socket: String,
    pub(super) pid: u32,
    pub(super) start_seconds: u64,
    pub(super) start_microseconds: u64,
    // Creation-time scope, never inferred from the current opening preference. Every
    // legacy record came from start_gui; new tabs in an existing GUI always write false.
    #[serde(default = "legacy_wezterm_owns_gui")]
    pub(super) owns_gui: bool,
}

const fn legacy_wezterm_owns_gui() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct WindowsProcessIdentity {
    pub(super) creation_time: u64,
    pub(super) executable_path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) enum CloseOutcome {
    Closed,
    Missing,
}

#[derive(Debug)]
pub(in crate::native) struct CloseResult {
    pub outcome: CloseOutcome,
    pub reason: Option<String>,
}

impl From<CloseOutcome> for CloseResult {
    fn from(outcome: CloseOutcome) -> Self {
        Self {
            outcome,
            reason: None,
        }
    }
}

#[derive(Debug)]
pub(super) struct TerminalSendFailure {
    error: anyhow::Error,
    delivery_may_have_occurred: bool,
}

impl TerminalSendFailure {
    pub(super) fn not_sent(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: false,
        }
    }

    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", test)),
        allow(dead_code)
    )]
    pub(super) fn delivery_uncertain(error: anyhow::Error) -> Self {
        Self {
            error,
            delivery_may_have_occurred: true,
        }
    }

    pub(super) fn delivery_may_have_occurred(&self) -> bool {
        self.delivery_may_have_occurred
    }

    pub(super) fn error(&self) -> &anyhow::Error {
        &self.error
    }

    pub(super) fn into_error(self) -> anyhow::Error {
        self.error
    }
}

pub(super) type TerminalSendResult = std::result::Result<(), TerminalSendFailure>;

/// The terminal for a launch whose provider runs on Herdr here (`herdr_runs_provider`, its
/// adapter's answer) or whose provider is not known yet (`None`, a reopen before it reads
/// its source). Only on Windows does it change the automatic choice: inside an hc terminal
/// it is Herdr when the provider runs there, and the managed console otherwise, as before
/// Herdr reached Windows. An explicit choice wins; admission then asks the provider.
/// Launches reach it through `session_start::choose_launch_surface`, which supplies the
/// answer.
pub(super) fn select_for(
    preferred: Option<TerminalKind>,
    herdr_runs_provider: Option<bool>,
) -> Result<TerminalKind> {
    #[cfg(windows)]
    return windows::select(preferred, herdr_runs_provider.unwrap_or(false));
    #[cfg(not(windows))]
    {
        let _ = herdr_runs_provider;
        platform::select(preferred)
    }
}

fn herdr_environment(marker: bool, term_program: Option<&str>) -> bool {
    marker && term_program.is_some_and(|value| value.eq_ignore_ascii_case("ghostty"))
}

/// The launch admission of a selected surface, before any session is created. A kind that
/// `select_for` chose from the caller's environment (`preferred` is the same request) proves
/// that environment here: inside Herdr the caller's `HC_TERMINAL_ID` must name a live terminal
/// of the running core that this process runs in. Selection itself reads only the
/// environment and opens nothing.
pub(super) fn admit_selection(
    preferred: Option<TerminalKind>,
    selected: TerminalKind,
) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if preferred.is_none() && selected == TerminalKind::Herdr {
        return herdr::verify_caller_terminal();
    }
    let _ = (preferred, selected);
    Ok(())
}

/// The command a user runs to watch or type in a surface that has no window of its own.
pub(in crate::native) fn attach_command(session: &TerminalSession) -> Option<String> {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    if session.kind == TerminalKind::Herdr {
        return herdr::attach_command(session);
    }
    #[cfg(target_os = "linux")]
    return linux::tmux::attach_command(session);
    #[cfg(target_os = "macos")]
    return None;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = session;
        None
    }
}

// `directory` is the private directory of the managed session. Windows and Warp use it
// for a one-writer host offer/start decision before the provider process can run.
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
    #[cfg(all(test, windows))]
    if let Some(error) = launch_failure_test_support::take_failure(directory) {
        return Err(error);
    }
    #[cfg(windows)]
    {
        windows_dispatch::open_bound_tab(
            windows_installed(),
            kind,
            command,
            directory,
            deadline,
            bind,
            unbind,
        )
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        posix::open_bound_tab(
            posix_installed(),
            kind,
            command,
            directory,
            deadline,
            bind,
            unbind,
        )
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (directory, deadline);
        let mut session = platform::open_tab(kind, command)?;
        if let Err(error) = bind(&mut session) {
            let cleanup = platform::close_session(&session);
            return match cleanup {
                Ok(_) => match unbind() {
                    Ok(()) => Err(error).context("failed to bind the created terminal surface"),
                    Err(unbind_error) => Err(anyhow::anyhow!(
                        "failed to bind the created terminal surface: {error:#}; surface cleanup succeeded but its durable binding could not be removed: {unbind_error:#}"
                    )),
                },
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "failed to bind the created terminal surface: {error:#}; exact surface cleanup also failed: {cleanup_error:#}"
                )),
            };
        }
        Ok(session)
    }
}

#[cfg(all(test, windows))]
pub(in crate::native) mod launch_failure_test_support;

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn bind_surface_before_start<T, Bind, Start, Cleanup, Unbind>(
    surface: &mut T,
    bind: Bind,
    start: Start,
    cleanup: Cleanup,
    unbind: Unbind,
) -> Result<()>
where
    Bind: FnOnce(&mut T) -> Result<()>,
    Start: FnOnce() -> Result<()>,
    Cleanup: FnOnce() -> Result<()>,
    Unbind: FnOnce() -> Result<()>,
{
    if let Err(error) = bind(surface) {
        return match cleanup() {
            Ok(()) => match unbind() {
                Ok(()) => Err(error),
                Err(unbind_error) => Err(anyhow::anyhow!(
                    "{error:#}; exact surface cleanup succeeded but its durable binding could not be removed: {unbind_error:#}"
                )),
            },
            Err(cleanup_error) => Err(cleanup_failure(error, cleanup_error)),
        };
    }
    if let Err(error) = start() {
        return match cleanup() {
            Ok(()) => match unbind() {
                Ok(()) => Err(error),
                Err(unbind_error) => Err(anyhow::anyhow!(
                    "{error:#}; exact surface cleanup succeeded but its durable binding could not be removed: {unbind_error:#}"
                )),
            },
            Err(cleanup_error) => Err(cleanup_failure(error, cleanup_error)),
        };
    }
    Ok(())
}

/// Time kept back from a launch's deadline for removing the exact surface it created.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
const STARTUP_CLEANUP_RESERVE: Duration = Duration::from_secs(2);

/// Creates a surface whose program waits for its binding, binds it, then starts it; a
/// binding or start that fails removes exactly the created surface. The startup deadline
/// keeps back time for that removal.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
pub(in crate::native::terminal) fn open_with(
    deadline: Instant,
    create: impl FnOnce(Instant) -> Result<TerminalSession>,
    start: impl FnOnce(&TerminalSession, Instant) -> Result<()>,
    cleanup: impl FnOnce(&TerminalSession, Instant) -> Result<()>,
    bind: impl FnOnce(&mut TerminalSession) -> Result<()>,
    unbind: impl FnOnce() -> Result<()>,
) -> Result<TerminalSession> {
    let startup_deadline = deadline
        .checked_sub(STARTUP_CLEANUP_RESERVE)
        .filter(|candidate| *candidate > Instant::now())
        .ok_or_else(|| {
            anyhow::anyhow!("terminal startup timeout leaves no room for exact surface cleanup")
        })?;
    let mut session = create(startup_deadline)?;
    let start_session = session.clone();
    let cleanup_session = session.clone();
    bind_surface_before_start(
        &mut session,
        bind,
        || start(&start_session, startup_deadline),
        || cleanup(&cleanup_session, deadline),
        unbind,
    )?;
    Ok(session)
}

// A cleanup that hands back its proven surface keeps that type, so that the launch failure
// records the handle for a later close; any other cleanup failure is one line of text.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn cleanup_failure(error: anyhow::Error, cleanup_error: anyhow::Error) -> anyhow::Error {
    if cleanup_error
        .downcast_ref::<RetainedLaunchSurface>()
        .is_some()
    {
        return cleanup_error.context(format!("{error:#}; exact surface cleanup also failed"));
    }
    anyhow::anyhow!("{error:#}; exact surface cleanup also failed: {cleanup_error:#}")
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    #[cfg(windows)]
    {
        windows_dispatch::send_file(windows_installed(), session, prompt_path, deadline)
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        posix::send_file(posix_installed(), session, prompt_path, deadline)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        // Unsupported-platform stubs never send, so not_sent is accurate.
        platform::send_file(session, prompt_path, deadline).map_err(TerminalSendFailure::not_sent)
    }
}

// Transport-neutral keys for an exact, unchanged managed dialog. Providers own
// recognition and authorization; the terminal only compares the captured screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(super) enum DialogKey {
    Enter,
    DownEnter,
}

#[derive(Deserialize, Serialize)]
pub(super) struct GuardedDialogInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) expected_claim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) expires_unix_ms: Option<u128>,
    pub(super) screen: String,
    pub(super) key: DialogKey,
}

pub(super) fn read_screen(session: &TerminalSession, deadline: Instant) -> Result<String> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return posix::read_screen(posix_installed(), session, deadline);
    #[cfg(windows)]
    {
        windows_dispatch::read_screen(windows_installed(), session, deadline)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (session, deadline);
        bail!("managed screen reads unsupported on this platform");
    }
}

pub(super) const fn guards_dialog_input(kind: TerminalKind) -> bool {
    facts(kind).guards_dialog_input
}

pub(super) fn guarded_dialog_input(
    directory: &Path,
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        macos_dialog::dispatch(directory, input, deadline, || {
            posix::guarded_dialog_input(posix_installed(), session, input, deadline)
        })
    }
    #[cfg(windows)]
    {
        windows_dispatch::guarded_dialog_input(
            windows_installed(),
            directory,
            session,
            input,
            deadline,
        )
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (directory, session, input, deadline);
        bail!("managed dialog input unsupported on this platform");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn verify_posix_surface(
    directory: &Path,
    session: &TerminalSession,
    timeout: Option<std::time::Duration>,
) -> Result<Option<String>> {
    posix::verify_surface(posix_installed(), directory, session, timeout)
}

/// The surface part of the ownership proof on Windows, as `verify_posix_surface` is
/// elsewhere: a Herdr terminal live on its recorded core; a managed console has none.
#[cfg(windows)]
pub(super) fn verify_windows_surface(
    session: &TerminalSession,
    timeout: Option<std::time::Duration>,
) -> Result<()> {
    windows_dispatch::verify_surface(windows_installed(), session, timeout)
}

pub(super) fn close_session_until(
    directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    crate::native::session::close::budget::close_remaining(deadline)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        posix::close_session_until(posix_installed(), directory, session, deadline)
    }
    #[cfg(windows)]
    {
        windows_dispatch::close_session_until(windows_installed(), directory, session, deadline)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        platform::close_session_until(directory, session, deadline)
    }
}

/// Read-only presence check. Unlike control authorization, absence and an inspection
/// error have different meanings. Never open a terminal app merely to diagnose it.
pub(super) fn surface_present(
    directory: &Path,
    session: &TerminalSession,
    timeout: std::time::Duration,
) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        posix::surface_present(posix_installed(), directory, session, timeout)
    }
    #[cfg(windows)]
    {
        windows_dispatch::surface_present(windows_installed(), directory, session, timeout)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (directory, session, timeout);
        bail!("terminal surface probes are unsupported on this platform")
    }
}

/// What the records of a session say that its console cannot: where the host of a tab
/// recorded itself, and whether the wrapper that the console root starts ever ran.
#[cfg(target_os = "windows")]
pub(super) struct WindowsSessionRecords<'a> {
    pub(super) directory: &'a Path,
    pub(super) root_never_ran: bool,
}

#[cfg(target_os = "windows")]
pub(super) use windows::console_control as windows_console_control;

#[cfg(target_os = "windows")]
pub(super) use windows::ownership::{windows_console_handle_path, windows_console_root_never_ran};

#[cfg(target_os = "windows")]
pub(super) use windows::powershell_executable as windows_powershell_executable;

#[cfg(all(target_os = "windows", test))]
pub(super) use windows::console_launch_command_line as windows_console_launch_command_line;

#[cfg(windows)]
pub(super) use windows::set_private_permissions as windows_set_private_permissions;

#[cfg(windows)]
pub(in crate::native) use windows::observe_process_identity as observe_windows_process_identity;

#[cfg(any(windows, test))]
#[path = "windows/incarnation.rs"]
mod windows_incarnation;
#[cfg(any(windows, test))]
pub(in crate::native) use windows_incarnation::WindowsIncarnation;

#[cfg(windows)]
pub(super) use windows::query_process_identity as windows_process_identity;

#[cfg(unix)]
pub(in crate::native) fn shell_quote(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(super) fn validate_terminal_send_budget(
    submit_count: usize,
    terminal_kind: TerminalKind,
    timeout: Duration,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows_budget::validate_terminal_send_budget_for_platform(
            submit_count,
            terminal_kind,
            timeout,
            true,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (submit_count, terminal_kind, timeout);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    #[cfg(target_os = "macos")]
    use super::macos;
    use super::{TerminalKind, TerminalSendFailure, TerminalSession, bind_surface_before_start};
    use anyhow::bail;

    #[test]
    fn terminal_send_failure_distinguishes_not_started_from_uncertain_delivery() {
        let not_started = TerminalSendFailure::not_sent(anyhow::anyhow!("deadline exhausted"));
        assert!(!not_started.delivery_may_have_occurred());
        assert_eq!(not_started.error().to_string(), "deadline exhausted");

        let uncertain =
            TerminalSendFailure::delivery_uncertain(anyhow::anyhow!("helper timed out"));
        assert!(uncertain.delivery_may_have_occurred());
        assert_eq!(uncertain.error().to_string(), "helper timed out");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_expired_send_deadline_is_confirmed_not_started() {
        let failure = super::send_file(
            &macos_test_iterm_session(),
            std::path::Path::new("/private/tmp/agent-bridge-expired-prompt"),
            std::time::Instant::now(),
        )
        .unwrap_err();

        assert!(!failure.delivery_may_have_occurred());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unrepresentable_prompt_path_is_confirmed_not_started() {
        use std::os::unix::ffi::OsStrExt;

        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
            b"/private/tmp/agent-bridge-prompt-\xff",
        ));
        let failure = super::send_file(
            &macos_test_iterm_session(),
            &path,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap_err();

        assert!(!failure.delivery_may_have_occurred());
    }

    #[cfg(target_os = "macos")]
    fn macos_test_iterm_session() -> TerminalSession {
        TerminalSession::new(TerminalKind::Iterm2, "test-session".to_owned())
    }

    #[cfg(windows)]
    fn absent_windows_herdr_handle() -> TerminalSession {
        TerminalSession::new(TerminalKind::Herdr, "t7".into())
            .with_herdr_core(
                super::HerdrSurface {
                    socket: format!(r"\\.\pipe\tabcli-absent-core-{}", std::process::id()),
                    core_id: "core-a".into(),
                    created_ms: 1,
                    host_pid: 4,
                    birth: super::record::HerdrHostBirth::Windows {
                        host_creation_time: 1,
                    },
                },
                "unused",
            )
            .with_binding(Some("session-herdr".into()))
    }

    // Every surface operation on a Windows Herdr handle goes to its recorded core, never to
    // the console helper: with no core there, each one names the missing core, a send is
    // known unsent, a close is unknown (never Missing), and a guarded dialog answer is
    // refused as Herdr declares.
    #[cfg(windows)]
    #[test]
    fn windows_herdr_handles_never_reach_the_console_helper() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-herdr");
        std::fs::create_dir(&directory).unwrap();
        let prompt = directory.join("prompt.txt");
        std::fs::write(&prompt, "hello").unwrap();
        let handle = absent_windows_herdr_handle();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let missing = "no Herdr Core is running";
        let failure = super::send_file(&handle, &prompt, deadline).unwrap_err();
        assert!(!failure.delivery_may_have_occurred());
        assert!(format!("{:#}", failure.error()).contains(missing));
        let error = super::read_screen(&handle, deadline).unwrap_err();
        assert!(format!("{error:#}").contains(missing), "{error:#}");
        let error = super::close_session_until(&directory, &handle, deadline).unwrap_err();
        assert!(format!("{error:#}").contains(missing), "{error:#}");
        let error = super::surface_present(&directory, &handle, std::time::Duration::from_secs(5))
            .unwrap_err();
        assert!(format!("{error:#}").contains(missing), "{error:#}");
        let input = super::GuardedDialogInput {
            expected_claim: None,
            expires_unix_ms: None,
            screen: String::new(),
            key: super::DialogKey::Enter,
        };
        assert_eq!(
            super::guarded_dialog_input(&directory, &handle, &input, deadline)
                .unwrap_err()
                .to_string(),
            TerminalKind::Herdr
                .registration()
                .guarded_input_refusal
                .unwrap()
        );
    }

    // The Windows ownership proof checks a Herdr terminal on its core before the owner, as
    // macOS and Linux do; a dead owner's Herdr handle may be closed like a console's.
    #[cfg(windows)]
    #[test]
    fn windows_herdr_ownership_proves_the_terminal_and_allows_a_dead_owner_close() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-herdr");
        std::fs::create_dir(&directory).unwrap();
        let handle = absent_windows_herdr_handle();
        let error = super::ownership::verified_terminal_surface_owner(
            &directory,
            "session-herdr",
            &handle,
            Some(std::time::Duration::from_secs(5)),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("no Herdr Core is running"),
            "{error:#}"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        assert_eq!(
            super::ownership::close_dead_owner_surface_with(
                &handle,
                |_| Ok(super::CloseOutcome::Closed),
                deadline,
            )
            .unwrap(),
            super::CloseOutcome::Closed
        );
        let foreign = TerminalSession::new(TerminalKind::Iterm2, "x".into());
        assert!(
            super::ownership::close_dead_owner_surface_with(
                &foreign,
                |_| panic!("a foreign handle is never closed"),
                deadline,
            )
            .is_err()
        );
    }

    #[test]
    fn explicit_terminal_names_have_stable_canonical_values() {
        for alias in ["iterm", "iTerm2", "iTerm.app"] {
            assert_eq!(TerminalKind::from_str(alias), Ok(TerminalKind::Iterm2));
        }
        for alias in ["terminal", "Terminal.app", "apple-terminal", "default"] {
            assert_eq!(
                TerminalKind::from_str(alias),
                Ok(TerminalKind::AppleTerminal)
            );
        }
        assert_eq!(TerminalKind::from_str("Ghostty"), Ok(TerminalKind::Ghostty));
        assert_eq!(TerminalKind::from_str("Warp"), Ok(TerminalKind::Warp));
        assert_eq!(
            TerminalKind::from_str("WarpTerminal"),
            Ok(TerminalKind::Warp)
        );
        assert_eq!(TerminalKind::from_str("WezTerm"), Ok(TerminalKind::WezTerm));
        assert_eq!(
            serde_json::to_value(TerminalKind::WezTerm).unwrap(),
            "wezterm"
        );
        for alias in ["herdr", "Herdr", "hc", "herdr-core"] {
            assert_eq!(TerminalKind::from_str(alias), Ok(TerminalKind::Herdr));
        }
        assert_eq!(serde_json::to_value(TerminalKind::Herdr).unwrap(), "herdr");
        assert_eq!(
            TerminalKind::Herdr.supported_on_this_platform(),
            cfg!(any(target_os = "macos", target_os = "linux", windows))
        );
        assert_eq!(
            TerminalKind::from_str("windows-console"),
            Ok(TerminalKind::WindowsConsole)
        );
        let error = TerminalKind::from_str("vscode").unwrap_err();
        assert!(error.contains("windows-console"), "{error}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_selects_its_managed_console_transport() {
        assert_eq!(
            super::windows::select_from(None, false, None, false).unwrap(),
            TerminalKind::WindowsConsole
        );
        assert_eq!(
            super::select_for(Some(TerminalKind::WindowsConsole), None).unwrap(),
            TerminalKind::WindowsConsole
        );
        assert!(super::select_for(Some(TerminalKind::Iterm2), None).is_err());
    }

    #[test]
    fn terminal_session_records_preserve_legacy_iterm_state() {
        let legacy: TerminalSession =
            serde_json::from_str(r#"{"iterm_session_id":"legacy-session"}"#).unwrap();
        assert_eq!(legacy.kind, TerminalKind::Iterm2);
        assert_eq!(legacy.id, "legacy-session");
        assert_eq!(legacy.tab_id, None);
        assert_eq!(legacy.window_id, None);
        assert_eq!(legacy.managed_session_id, None);

        let ghostty = TerminalSession::new(TerminalKind::Ghostty, "terminal-id".to_owned())
            .with_location(Some("tab-id".to_owned()), Some("window-id".to_owned()));
        assert_eq!(
            serde_json::to_value(&ghostty).unwrap(),
            serde_json::json!({
                "terminal": "ghostty",
                "session_id": "terminal-id",
                "tab_id": "tab-id",
                "window_id": "window-id"
            })
        );

        let bound: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal",
            "session_id": "/dev/ttys001",
            "window_id": "1001",
            "managed_session_id": "session-owner123"
        }))
        .unwrap();
        assert!(bound.verify_managed_session("session-owner123").is_ok());
        assert!(bound.verify_managed_session("session-other456").is_err());
        let bound = serde_json::to_value(bound).unwrap();
        assert_eq!(bound["managed_session_id"], "session-owner123");
        assert!(ghostty.verify_managed_session("session-owner123").is_err());
    }

    #[test]
    fn a_wezterm_record_keeps_the_process_that_issued_its_pane_id() {
        let record = serde_json::json!({
            "terminal": "wezterm",
            "session_id": "7",
            "managed_session_id": "session-owner123",
            "wezterm_mux": {
                "socket": "/Users/tester/.local/share/wezterm/gui-sock-4242",
                "pid": 4242,
                "start_seconds": 1_790_000_000_u64,
                "start_microseconds": 5
            }
        });
        let session: TerminalSession = serde_json::from_value(record.clone()).unwrap();
        assert_eq!(session.kind, TerminalKind::WezTerm);
        assert!(session.wezterm_mux().unwrap().owns_gui);
        let mut explicit = record;
        explicit["wezterm_mux"]["owns_gui"] = serde_json::json!(true);
        assert_eq!(serde_json::to_value(&session).unwrap(), explicit);
        explicit["wezterm_mux"]["owns_gui"] = serde_json::json!(false);
        explicit["tab_id"] = serde_json::json!("8");
        explicit["window_id"] = serde_json::json!("3");
        let shared: TerminalSession = serde_json::from_value(explicit.clone()).unwrap();
        assert!(!shared.wezterm_mux().unwrap().owns_gui);
        assert_eq!(serde_json::to_value(shared).unwrap(), explicit);
    }

    #[test]
    fn suspended_surface_is_bound_before_start_and_cleaned_on_bind_failure() {
        use std::cell::RefCell;

        let steps = RefCell::new(Vec::new());
        bind_surface_before_start(
            &mut (),
            |_| {
                steps.borrow_mut().push("bind");
                Ok(())
            },
            || {
                steps.borrow_mut().push("start");
                Ok(())
            },
            || {
                steps.borrow_mut().push("cleanup");
                Ok(())
            },
            || {
                steps.borrow_mut().push("unbind");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*steps.borrow(), ["bind", "start"]);

        steps.borrow_mut().clear();
        assert!(
            bind_surface_before_start(
                &mut (),
                |_| {
                    steps.borrow_mut().push("bind");
                    bail!("persist failed")
                },
                || {
                    steps.borrow_mut().push("start");
                    Ok(())
                },
                || {
                    steps.borrow_mut().push("cleanup");
                    Ok(())
                },
                || {
                    steps.borrow_mut().push("unbind");
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(*steps.borrow(), ["bind", "cleanup", "unbind"]);

        steps.borrow_mut().clear();
        assert!(
            bind_surface_before_start(
                &mut (),
                |_| {
                    steps.borrow_mut().push("bind");
                    Ok(())
                },
                || {
                    steps.borrow_mut().push("start");
                    bail!("start failed")
                },
                || {
                    steps.borrow_mut().push("cleanup");
                    bail!("cleanup failed")
                },
                || {
                    steps.borrow_mut().push("unbind");
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(*steps.borrow(), ["bind", "start", "cleanup"]);
    }

    #[test]
    fn terminal_records_discard_legacy_visible_title_metadata() {
        let session: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal",
            "session_id": "/dev/ttys001",
            "window_id": "1001",
            "managed_session_id": "session-owner123",
            "ownership_title": "legacy-visible-marker"
        }))
        .unwrap();

        let record = serde_json::to_value(session).unwrap();
        assert!(record.get("ownership_title").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_open_uses_separate_create_discover_and_frame_transactions() {
        fn assert_ordered(script: &str, statements: &[&str]) {
            let mut remaining = script;
            for statement in statements {
                let offset = remaining.find(statement).unwrap_or_else(|| {
                    panic!("missing or out-of-order Ghostty statement: {statement}")
                });
                remaining = &remaining[offset + statement.len()..];
            }
        }

        let version = macos::ghostty::VERSION_SCRIPT;
        assert!(version.contains("return version"));
        assert!(!version.contains("new tab"));
        assert!(!version.contains("new window"));

        let create = macos::ghostty::CREATE_SURFACE_SCRIPT;
        // Creation returns IDs from the native result, never a snapshot delta.
        // The adapter's injected creation tests also reject old/wrong/lost IDs.
        assert_ordered(
            create,
            &[
                "set targetWindow to new window with configuration cfg",
                "set targetTab to selected tab of targetWindow",
                "set matches to every window whose id is wantedWindowId",
                "if (count of matches) is not 1 then error",
                "set targetTab to new tab in targetWindow with configuration cfg",
                "set createdWindowId to id of targetWindow",
                "set createdTabId to id of targetTab",
                "if (count of terminals of targetTab) is 1 then set terminalId to id of focused terminal of targetTab",
                "return createdTabId & linefeed & createdWindowId & linefeed & terminalId",
            ],
        );
        assert!(!create.contains("input text"));
        assert!(!create.contains("send key"));
        assert!(!create.contains("bridgeCommand"));

        let discover = macos::ghostty::DISCOVER_TERMINAL_SCRIPT;
        // Metadata is insufficient: probe the exact sole returned terminal with
        // empty input before reporting readiness. No provider command or Enter.
        assert_ordered(
            discover,
            &[
                "set wantedTabId to item 1 of argv",
                "set wantedWindowId to item 2 of argv",
                "set expectedTerminalId to item 3 of argv",
                "set ws to every window whose id is wantedWindowId",
                "if (count of ws) is not 1 then error",
                "set targetWindow to item 1 of ws",
                "set ts to every tab of targetWindow whose id is wantedTabId",
                "if (count of ts) is not 1 then error",
                "set targetTab to item 1 of ts",
                "if (count of terminals of targetTab) is 0 then return \"not-ready\"",
                "if (count of terminals of targetTab) is not 1 then error",
                "set targetTerminal to focused terminal of targetTab",
                "set terminalId to id of targetTerminal",
                "if expectedTerminalId is not \"-\" and terminalId is not expectedTerminalId then error",
                "input text \"\" to targetTerminal",
                "if errorNumber is -10000 and errorText contains \"Terminal surface model is not available\" then return \"not-ready\"",
                "return \"ready\" & linefeed & terminalId",
            ],
        );
        assert_eq!(discover.matches("input text ").count(), 1);
        assert!(!discover.contains("new tab"));
        assert!(!discover.contains("new window"));
        assert!(!discover.contains("bridgeCommand"));
        assert!(!discover.contains("send key"));

        // The start writes one frame for the launch host to the exact terminal and
        // sends no Enter: nothing that reaches the terminal is a shell line.
        let queue = macos::ghostty::QUEUE_COMMAND_SCRIPT;
        assert_ordered(
            queue,
            &[
                "set wantedTerminalId to item 1 of argv",
                "set wantedTabId to item 2 of argv",
                "set wantedWindowId to item 3 of argv",
                "set targetWindow to first window whose id is wantedWindowId",
                "repeat with candidateTab in tabs of targetWindow",
                "if id of candidateTab is wantedTabId then",
                "if targetTab is missing value then return \"missing\"",
                "repeat with candidateTerminal in terminals of targetTab",
                "if id of candidateTerminal is wantedTerminalId then",
                "if targetTerminal is missing value then return \"missing\"",
                "input text bridgeCommand to targetTerminal",
                "return \"queued\"",
            ],
        );
        assert!(!queue.contains("new tab"));
        assert!(!queue.contains("new window"));
        assert!(!queue.contains("focused terminal"));
        assert!(queue.contains("set bridgeCommand to item 4 of argv"));
        assert!(queue.contains("if errorNumber is -10000 then return \"not-ready\""));
        assert_eq!(queue.matches("input text ").count(), 1);
        assert!(!queue.contains("send key"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_adapter_targets_stable_ids_without_interpolating_input() {
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("new tab in targetWindow"));
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("id of targetTab"));
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("id of targetWindow"));
        assert!(macos::ghostty::DISCOVER_TERMINAL_SCRIPT.contains("item 1 of argv"));
        assert!(macos::ghostty::DISCOVER_TERMINAL_SCRIPT.contains("id of targetTerminal"));
        assert!(macos::ghostty::QUEUE_COMMAND_SCRIPT.contains("item 4 of argv"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("input text promptText"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("send key \"enter\""));
        assert!(macos::ghostty::CLOSE_TAB_SCRIPT.contains("close tab targetTab"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_open_never_uses_surface_configuration_initial_input() {
        let create = macos::ghostty::CREATE_SURFACE_SCRIPT;
        assert_eq!(create.matches("new surface configuration").count(), 1);
        let configured_properties: Vec<_> = create
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("set ") && line.contains(" of cfg to "))
            .collect();
        // The command is the launch host, given as an argument; the creation knows
        // neither the session nor its launch command.
        assert_eq!(
            configured_properties,
            ["set command of cfg to item 2 of argv"]
        );
        assert_eq!(create.matches("with configuration cfg").count(), 2);
        assert!(!create.contains("item 3 of argv"));
        assert!(!create.contains("bridgeCommand"));
        assert!(!create.contains("input text"));
        assert!(!create.contains("send key"));
        for script in [
            create,
            macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            macos::ghostty::QUEUE_COMMAND_SCRIPT,
        ] {
            assert!(!script.contains("initial input"));
            assert!(!script.contains("bridgeConfiguration"));
        }
        for script in [
            macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            macos::ghostty::QUEUE_COMMAND_SCRIPT,
        ] {
            assert!(!script.contains("new surface configuration"));
            assert!(!script.contains("with configuration"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apple_terminal_adapter_targets_only_its_created_tty() {
        // The start is typed once, by the creation, into the tab that the creation
        // returns: no script addresses a tab with the start command afterwards.
        assert!(
            macos::apple_terminal::OPEN_TAB_SCRIPT.contains("set bridgeCommand to item 1 of argv")
        );
        assert_eq!(
            macos::apple_terminal::OPEN_TAB_SCRIPT
                .lines()
                .map(str::trim)
                .filter(|line| !line.starts_with("--") && line.contains("do script"))
                .collect::<Vec<_>>(),
            ["set targetTab to do script ((character id 21) & bridgeCommand)"]
        );
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("System Events"));
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("front window"));
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("selected tab"));
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("tty of targetTab"));
        assert!(
            macos::apple_terminal::OPEN_TAB_SCRIPT
                .contains("windowIdForTty(targetTty, priorWindowIds)")
        );
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("id of targetWindow"));
        assert!(
            macos::apple_terminal::SEND_FILE_SCRIPT.contains("tty of candidateTab is wantedTty")
        );
        assert!(
            macos::apple_terminal::SEND_FILE_SCRIPT.contains("do script promptText in targetTab")
        );
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("character id 3"));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("do script controlC"));
        assert!(macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("repeat 60 times"));
        assert!(
            macos::apple_terminal::CLOSE_TAB_SCRIPT
                .contains("if (count of tabs of targetWindow) is not 1 then error")
        );
        assert_eq!(
            macos::apple_terminal::CLOSE_TAB_SCRIPT
                .matches("if (count of tabs of targetWindow) is not 1 then error")
                .count(),
            2,
            "Terminal.app must re-check the one-tab invariant immediately before close"
        );
        let close_script = macos::apple_terminal::CLOSE_TAB_SCRIPT;
        let process_stopped = close_script
            .find("if busy of targetTab then error")
            .expect("busy-process finality check");
        let final_window = close_script
            .rfind("if id of targetWindow is not wantedWindowId then error")
            .expect("final window identity check");
        let final_tty = close_script
            .rfind("if not my isOwnedTty(tty of targetTab, wantedTty) then error")
            .expect("final tty identity check");
        let final_tab_count = close_script
            .rfind("if (count of tabs of targetWindow) is not 1 then error")
            .expect("final one-tab check");
        let close_window = close_script
            .rfind("close targetWindow")
            .expect("native window close");
        assert!(
            process_stopped < final_window
                && final_window < final_tty
                && final_tty < final_tab_count
                && final_tab_count < close_window,
            "Terminal.app must repeat the full proof in order immediately before close"
        );
        assert!(macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("close targetWindow"));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("System Events"));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("keystroke \"w\""));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("close targetTab"));
    }
}
