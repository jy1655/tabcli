use std::{path::Path, str::FromStr, time::Instant};

#[cfg(not(target_os = "macos"))]
use anyhow::Context;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported as platform;
#[cfg(target_os = "windows")]
use windows as platform;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TerminalKind {
    Iterm2,
    AppleTerminal,
    Ghostty,
    WindowsConsole,
}

impl TerminalKind {
    pub(super) const fn supported_on_this_platform(self) -> bool {
        match self {
            Self::Iterm2 | Self::AppleTerminal => cfg!(target_os = "macos"),
            Self::WindowsConsole => cfg!(windows),
            // Ghostty cannot establish creation-time surface ownership in this build.
            Self::Ghostty => false,
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Iterm2 => "iterm2",
            Self::AppleTerminal => "apple-terminal",
            Self::Ghostty => "ghostty",
            Self::WindowsConsole => "windows-console",
        }
    }

    pub(super) const fn display_name(self) -> &'static str {
        match self {
            Self::Iterm2 => "iTerm2",
            Self::AppleTerminal => "Terminal.app",
            Self::Ghostty => "Ghostty",
            Self::WindowsConsole => "Windows Console",
        }
    }
}

impl FromStr for TerminalKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "ghostty" => Ok(Self::Ghostty),
            "windows-console" | "windows" | "console" => Ok(Self::WindowsConsole),
            "iterm" | "iterm.app" | "iterm2" => Ok(Self::Iterm2),
            "apple-terminal" | "apple_terminal" | "default" | "terminal" | "terminal.app" => {
                Ok(Self::AppleTerminal)
            }
            _ => Err(format!(
                "unsupported terminal {value:?}; expected ghostty, iterm2, terminal, or windows-console"
            )),
        }
    }
}

const fn legacy_iterm2_kind() -> TerminalKind {
    TerminalKind::Iterm2
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct TerminalSession {
    #[serde(default = "legacy_iterm2_kind", rename = "terminal")]
    pub(super) kind: TerminalKind,
    #[serde(alias = "iterm_session_id", rename = "session_id")]
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) managed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) windows_process_identity: Option<WindowsProcessIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct WindowsProcessIdentity {
    pub(super) creation_time: u64,
    pub(super) executable_path: String,
}

// The answer of comparing a live Windows process with a recorded identity. `Mismatch` is a
// confirmed observation (the pid is in use by a different process, so the recorded one is
// gone); a process that cannot be inspected is reported as an error by the caller, never as
// either variant.
#[cfg(any(windows, test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WindowsProcessIdentityCheck {
    Matches,
    Mismatch(&'static str),
}

impl TerminalSession {
    pub(super) fn verify_managed_session(&self, expected: &str) -> Result<()> {
        match self.managed_session_id.as_deref() {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => {
                bail!("terminal handle belongs to managed session {actual}, not {expected}")
            }
            None if self.kind == TerminalKind::Iterm2 => Ok(()),
            None => bail!("terminal handle is missing its managed session binding"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) enum CloseOutcome {
    Closed,
    Missing,
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

#[cfg(any(windows, test))]
pub(super) fn windows_console_helper_reports_missing(message: &str) -> bool {
    matches!(
        message.trim(),
        "console process is no longer available" | "Error: console process is no longer available"
    )
}

#[cfg(any(windows, test))]
pub(super) fn windows_console_helper_reports_send_not_started(message: &str) -> bool {
    matches!(
        message.trim(),
        "Windows console submission delays do not fit inside the remaining turn timeout"
            | "Error: Windows console submission delays do not fit inside the remaining turn timeout"
    )
}

#[cfg(any(windows, test))]
pub(super) fn windows_console_extra_submit_delay() -> std::time::Duration {
    std::time::Duration::from_secs(2)
}

#[cfg(any(windows, test))]
pub(super) fn windows_console_immediate_submit_count(submit_count: usize) -> usize {
    usize::from(submit_count == 1)
}

#[cfg(any(windows, test))]
pub(super) fn windows_console_submit_delays_fit(
    submit_count: usize,
    budget: std::time::Duration,
) -> bool {
    let delayed = submit_count.saturating_sub(windows_console_immediate_submit_count(submit_count));
    u32::try_from(delayed)
        .ok()
        .and_then(|count| windows_console_extra_submit_delay().checked_mul(count))
        .is_some_and(|required| required < budget)
}

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    platform::select(preferred)
}

// `directory` is the private directory of the managed session. Only Windows uses it: a
// Windows Terminal tab creates its own process, which hands the console root over through
// records kept there.
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
    #[cfg(windows)]
    {
        windows::open_bound_tab(kind, command, directory, deadline, bind, unbind)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = directory;
        macos::open_bound_tab(kind, command, deadline, bind, unbind)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
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

#[cfg(any(windows, target_os = "macos", test))]
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
            Err(cleanup_error) => Err(anyhow::anyhow!(
                "{error:#}; exact surface cleanup also failed: {cleanup_error:#}"
            )),
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
            Err(cleanup_error) => Err(anyhow::anyhow!(
                "{error:#}; exact surface cleanup also failed: {cleanup_error:#}"
            )),
        };
    }
    Ok(())
}

pub(super) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    #[cfg(windows)]
    {
        windows::send_file(session, prompt_path, deadline)
    }
    #[cfg(target_os = "macos")]
    {
        macos::send_file(session, prompt_path, deadline)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
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
    pub(super) screen: String,
    pub(super) key: DialogKey,
}

pub(super) fn read_screen(session: &TerminalSession, deadline: Instant) -> Result<String> {
    #[cfg(target_os = "macos")]
    return macos::read_screen(session, deadline);
    #[cfg(windows)]
    return windows::read_screen(session, deadline);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (session, deadline);
        bail!("managed screen reads unsupported on this platform");
    }
}

pub(super) fn guarded_dialog_input(
    session: &TerminalSession,
    input: &GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    #[cfg(target_os = "macos")]
    return macos::guarded_dialog_input(session, input, deadline);
    #[cfg(windows)]
    return windows::guarded_dialog_input(session, input, deadline);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (session, input, deadline);
        bail!("managed dialog input unsupported on this platform");
    }
}

#[cfg(any(windows, test))]
pub(super) fn remaining_send_budget_at(
    deadline: Instant,
    now: Instant,
) -> Result<std::time::Duration> {
    match deadline.checked_duration_since(now) {
        Some(remaining) if !remaining.is_zero() => Ok(remaining),
        _ => bail!("terminal send deadline is exhausted"),
    }
}

#[cfg(target_os = "macos")]
pub(super) fn verify_macos_surface(
    session: &TerminalSession,
    timeout: Option<std::time::Duration>,
) -> Result<Option<String>> {
    macos::verify_surface(session, timeout)
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    platform::close_session(session)
}

/// Read-only presence check. Unlike control authorization, absence and an inspection
/// error have different meanings. Never open a terminal app merely to diagnose it.
pub(super) fn surface_present(
    session: &TerminalSession,
    timeout: std::time::Duration,
) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::surface_present(session, timeout)
    }
    #[cfg(windows)]
    {
        let _ = timeout;
        let pid: u32 = session.id.parse().context("invalid Windows console pid")?;
        if !agent_bridge::process_is_alive(pid) {
            return Ok(false);
        }
        let identity = session
            .windows_process_identity
            .as_ref()
            .context("console identity is missing")?;
        Ok(windows_process_identity(pid)? == *identity)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (session, timeout);
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
pub(super) fn windows_console_control(
    action: &str,
    session: &TerminalSession,
    input_path: Option<&Path>,
    submit_count: usize,
    timeout: Option<std::time::Duration>,
    records: &WindowsSessionRecords<'_>,
) -> Result<()> {
    windows::console_control(action, session, input_path, submit_count, timeout, records)
}

#[cfg(target_os = "windows")]
pub(super) fn windows_console_host(directory: &Path) -> Result<()> {
    windows::run_console_host(directory)
}

#[cfg(target_os = "windows")]
pub(super) fn windows_powershell_executable() -> Result<std::path::PathBuf> {
    windows::powershell_executable()
}

#[cfg(all(target_os = "windows", test))]
pub(super) fn windows_console_launch_command_line(
    powershell: &Path,
    command: &str,
) -> Result<String> {
    windows::console_launch_command_line(powershell, command)
}

#[cfg(windows)]
pub(super) fn windows_set_private_permissions(path: &Path, directory: bool) -> Result<()> {
    windows::set_private_permissions(path, directory)
}

#[cfg(windows)]
pub(super) fn windows_process_identity(pid: u32) -> Result<WindowsProcessIdentity> {
    windows::query_process_identity(pid)
}

#[cfg(windows)]
pub(super) fn verify_windows_process_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<()> {
    windows::verify_process_identity(pid, identity.creation_time, &identity.executable_path)
}

// Distinguishes a confirmed identity mismatch from a process that cannot be inspected; see
// `WindowsProcessIdentityCheck`.
#[cfg(windows)]
pub(super) fn check_windows_process_identity(
    pid: u32,
    identity: &WindowsProcessIdentity,
) -> Result<WindowsProcessIdentityCheck> {
    windows::check_process_identity(pid, identity)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn classify_macos_terminal(
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> Option<TerminalKind> {
    if let Some(value) = term_program {
        return match value.to_ascii_lowercase().as_str() {
            "ghostty" => Some(TerminalKind::Ghostty),
            "iterm" | "iterm.app" | "iterm2" => Some(TerminalKind::Iterm2),
            "apple_terminal" | "terminal" | "terminal.app" => Some(TerminalKind::AppleTerminal),
            _ => None,
        };
    }
    if term.is_some_and(|value| value.eq_ignore_ascii_case("xterm-ghostty")) {
        return Some(TerminalKind::Ghostty);
    }
    if has_iterm_session_id {
        return Some(TerminalKind::Iterm2);
    }
    has_term_session_id.then_some(TerminalKind::AppleTerminal)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn select_macos_terminal(
    preferred: Option<TerminalKind>,
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> TerminalKind {
    if let Some(preferred) = preferred {
        return preferred;
    }
    match classify_macos_terminal(
        term_program,
        term,
        has_iterm_session_id,
        has_term_session_id,
    ) {
        // Ghostty is explicitly unsupported in v0.0.7. Auto-detection falls back to the
        // supported Terminal.app adapter; an explicit --terminal ghostty still fails closed.
        Some(TerminalKind::Ghostty) | None => TerminalKind::AppleTerminal,
        Some(kind) => kind,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    #[cfg(target_os = "macos")]
    use super::macos;
    use super::{
        TerminalKind, TerminalSendFailure, TerminalSession, bind_surface_before_start,
        classify_macos_terminal, select_macos_terminal,
        windows_console_helper_reports_send_not_started,
    };
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
        TerminalSession {
            kind: TerminalKind::Iterm2,
            id: "test-session".to_owned(),
            tab_id: None,
            window_id: None,
            managed_session_id: None,
            windows_process_identity: None,
        }
    }

    #[test]
    fn windows_helper_budget_rejection_proves_send_never_started() {
        assert!(windows_console_helper_reports_send_not_started(
            "Error: Windows console submission delays do not fit inside the remaining turn timeout"
        ));
        assert!(!windows_console_helper_reports_send_not_started(
            "Error: Windows console control helper timed out"
        ));
    }

    #[test]
    fn macos_terminal_detection_recognizes_each_supported_host() {
        assert_eq!(
            classify_macos_terminal(Some("ghostty"), Some("xterm-ghostty"), false, true),
            Some(TerminalKind::Ghostty)
        );
        assert_eq!(
            classify_macos_terminal(Some("iTerm.app"), Some("xterm-256color"), true, true),
            Some(TerminalKind::Iterm2)
        );
        assert_eq!(
            classify_macos_terminal(Some("Apple_Terminal"), Some("xterm-256color"), false, true),
            Some(TerminalKind::AppleTerminal)
        );
        assert_eq!(
            classify_macos_terminal(Some("vscode"), Some("xterm-256color"), false, false),
            None
        );
        assert_eq!(
            classify_macos_terminal(Some("vscode"), Some("xterm-ghostty"), false, true),
            None
        );
    }

    #[test]
    fn macos_terminal_selection_honors_explicit_choice_and_falls_back_safely() {
        assert_eq!(
            select_macos_terminal(
                Some(TerminalKind::Ghostty),
                Some("Apple_Terminal"),
                Some("xterm-256color"),
                false,
                true,
            ),
            TerminalKind::Ghostty
        );
        assert_eq!(
            select_macos_terminal(None, Some("vscode"), Some("xterm-256color"), false, false,),
            TerminalKind::AppleTerminal
        );
        assert_eq!(
            select_macos_terminal(None, None, None, false, false),
            TerminalKind::AppleTerminal
        );
        assert_eq!(
            select_macos_terminal(None, Some("ghostty"), Some("xterm-ghostty"), false, false,),
            TerminalKind::AppleTerminal
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
        assert_eq!(super::select(None).unwrap(), TerminalKind::WindowsConsole);
        assert_eq!(
            super::select(Some(TerminalKind::WindowsConsole)).unwrap(),
            TerminalKind::WindowsConsole
        );
        assert!(super::select(Some(TerminalKind::Iterm2)).is_err());
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

        let ghostty = TerminalSession {
            kind: TerminalKind::Ghostty,
            id: "terminal-id".to_owned(),
            tab_id: Some("tab-id".to_owned()),
            window_id: Some("window-id".to_owned()),
            managed_session_id: None,
            windows_process_identity: None,
        };
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
    fn ghostty_open_uses_separate_create_discover_queue_and_enter_transactions() {
        let version = macos::ghostty::VERSION_SCRIPT;
        assert!(version.contains("return version"));
        assert!(!version.contains("new tab"));
        assert!(!version.contains("new window"));

        let create = macos::ghostty::CREATE_SURFACE_SCRIPT;
        assert!(create.contains("set targetWindow to new window"));
        assert!(create.contains("set targetTab to new tab in targetWindow"));
        assert!(create.contains("id of targetTab"));
        assert!(create.contains("id of targetWindow"));
        assert!(!create.contains("focused terminal"));
        assert!(!create.contains("input text"));
        assert!(!create.contains("send key"));

        let discover = macos::ghostty::DISCOVER_TERMINAL_SCRIPT;
        assert!(discover.contains("first window whose id is wantedWindowId"));
        assert!(discover.contains("if id of candidateTab is wantedTabId then"));
        assert!(discover.contains("focused terminal of targetTab"));
        assert!(discover.contains("return \"not-ready\""));
        assert!(!discover.contains("new tab"));
        assert!(!discover.contains("input text"));
        assert!(!discover.contains("send key"));

        let queue = macos::ghostty::QUEUE_COMMAND_SCRIPT;
        for proof in ["wantedTerminalId", "wantedTabId", "wantedWindowId"] {
            assert!(queue.contains(proof));
        }
        assert!(queue.contains("input text bridgeCommand to targetTerminal"));
        assert!(queue.contains("if errorNumber is -10000 then return \"not-ready\""));
        assert_eq!(
            queue
                .matches("input text bridgeCommand to targetTerminal")
                .count(),
            1
        );
        assert!(!queue.contains("send key"));
        assert!(!queue.contains("new tab"));
        assert!(!queue.contains("focused terminal"));

        let press_enter = macos::ghostty::PRESS_ENTER_SCRIPT;
        for proof in ["wantedTerminalId", "wantedTabId", "wantedWindowId"] {
            assert!(press_enter.contains(proof));
        }
        assert!(!press_enter.contains("input text"));
        assert_eq!(
            press_enter
                .matches("send key \"enter\" to targetTerminal")
                .count(),
            1
        );
        assert!(!press_enter.contains("new tab"));
        assert!(!press_enter.contains("focused terminal"));
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
        assert!(macos::ghostty::PRESS_ENTER_SCRIPT.contains("item 3 of argv"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("input text promptText"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("send key \"enter\""));
        assert!(macos::ghostty::CLOSE_TAB_SCRIPT.contains("close tab targetTab"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_open_never_uses_surface_configuration_initial_input() {
        for script in [
            macos::ghostty::CREATE_SURFACE_SCRIPT,
            macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            macos::ghostty::QUEUE_COMMAND_SCRIPT,
            macos::ghostty::PRESS_ENTER_SCRIPT,
        ] {
            assert!(!script.contains("new surface configuration"));
            assert!(!script.contains("initial input"));
            assert!(!script.contains("bridgeConfiguration"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apple_terminal_adapter_targets_only_its_created_tty() {
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("set targetTab to do script \"\""));
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("bridgeCommand"));
        assert!(macos::apple_terminal::START_SESSION_SCRIPT.contains("item 1 of argv"));
        assert!(macos::apple_terminal::START_SESSION_SCRIPT.contains("item 3 of argv"));
        assert!(
            macos::apple_terminal::START_SESSION_SCRIPT
                .contains("do script bridgeCommand in targetTab")
        );
        assert!(
            !macos::apple_terminal::START_SESSION_SCRIPT
                .contains("do script bridgeCommand in front window")
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
            .rfind("if id of targetWindow is not wantedWindowId then return \"missing\"")
            .expect("final window identity check");
        let final_tty = close_script
            .rfind("if tty of targetTab is not wantedTty then return \"missing\"")
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
