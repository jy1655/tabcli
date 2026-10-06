use super as terminal;
#[cfg(target_os = "macos")]
use super::macos::process::{
    current_terminal_tty, live_native_process_identity, terminal_tty_device,
};
#[cfg(test)]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "macos", test))]
use anyhow::bail;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::path::Path;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(in crate::native) struct NativeSessionOwner {
    pub(in crate::native) pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) managed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_tty_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_start_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_start_microseconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_shell: Option<MacTerminalShellIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) terminal_app: Option<MacTerminalAppIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::native) windows_process_identity: Option<terminal::WindowsProcessIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct MacTerminalShellIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) process_group: u32,
    pub(in crate::native) terminal_tty_device: u64,
    pub(in crate::native) process_start_seconds: u64,
    pub(in crate::native) process_start_microseconds: u64,
}

// Captured from the verified native owner's ancestor chain before provider start.
// A Terminal window id and tty are identities only inside this app incarnation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct MacTerminalAppIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) start_seconds: u64,
    pub(in crate::native) start_microseconds: u64,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) struct NativeProcessIdentity {
    pub(in crate::native) pid: u32,
    pub(in crate::native) parent_pid: u32,
    pub(in crate::native) terminal_tty_device: u64,
    pub(in crate::native) process_group: u32,
    pub(in crate::native) terminal_process_group: u32,
    pub(in crate::native) process_start_seconds: u64,
    pub(in crate::native) process_start_microseconds: u64,
}

#[cfg(test)]
pub(in crate::native) fn verify_terminal_owner_attestation(
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
    surface_tty_device: u64,
) -> Result<()> {
    if session.kind != terminal::TerminalKind::AppleTerminal {
        bail!("native-session TTY attestation is only valid for Terminal.app")
    }
    session.verify_managed_session(expected_session_id)?;
    if session.window_id.as_deref().is_none_or(str::is_empty) {
        bail!("Terminal.app session record is missing its dedicated window id")
    }
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    if owner.terminal_tty.as_deref() != Some(session.id.as_str()) {
        bail!("native-session owner is attached to a different terminal TTY")
    }
    if owner.pid != live.pid {
        bail!("native-session owner PID no longer identifies the live process")
    }
    let owner_tty_device = owner
        .terminal_tty_device
        .context("native-session owner is missing its controlling TTY device")?;
    if owner_tty_device != live.terminal_tty_device || owner_tty_device != surface_tty_device {
        bail!("Terminal.app TTY no longer belongs to the native-session owner")
    }
    let owner_start_seconds = owner
        .process_start_seconds
        .context("native-session owner is missing its process start time")?;
    let owner_start_microseconds = owner
        .process_start_microseconds
        .context("native-session owner is missing its process start time")?;
    if owner_start_seconds != live.process_start_seconds
        || owner_start_microseconds != live.process_start_microseconds
    {
        bail!("native-session owner PID was reused by another process")
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    let live = live_native_process_identity(pid)?;
    let terminal_tty = current_terminal_tty()?;
    let terminal_tty_device = terminal_tty_device(Path::new(&terminal_tty))?;
    if live.terminal_tty_device != terminal_tty_device {
        bail!("native-session process is not attached to its reported terminal TTY")
    }
    let live_shell = live_native_process_identity(live.parent_pid)?;
    let terminal_shell = MacTerminalShellIdentity {
        pid: live_shell.pid,
        process_group: live_shell.process_group,
        terminal_tty_device: live_shell.terminal_tty_device,
        process_start_seconds: live_shell.process_start_seconds,
        process_start_microseconds: live_shell.process_start_microseconds,
    };
    let owner = NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        terminal_tty: Some(terminal_tty),
        terminal_tty_device: Some(terminal_tty_device),
        process_start_seconds: Some(live.process_start_seconds),
        process_start_microseconds: Some(live.process_start_microseconds),
        process_group: Some(live.process_group),
        terminal_process_group: Some(live.terminal_process_group),
        terminal_shell: Some(terminal_shell),
        terminal_app: None,
        windows_process_identity: None,
    };
    verified_terminal_owner_process_group(&owner, &live)?;
    verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    Ok(owner)
}

#[cfg(windows)]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    Ok(NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        windows_process_identity: Some(terminal::windows_process_identity(pid)?),
        ..NativeSessionOwner::default()
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(in crate::native) fn current_native_session_owner(
    session_id: &str,
) -> Result<NativeSessionOwner> {
    Ok(NativeSessionOwner {
        pid: std::process::id(),
        managed_session_id: Some(session_id.to_owned()),
        ..NativeSessionOwner::default()
    })
}

#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn native_owner_identity_matches(
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
) -> bool {
    if owner.pid != live.pid {
        return false;
    }
    match (
        owner.terminal_tty_device,
        owner.process_start_seconds,
        owner.process_start_microseconds,
        owner.process_group,
        owner.terminal_process_group,
    ) {
        (None, None, None, None, None) => true,
        (
            Some(terminal_tty_device),
            Some(process_start_seconds),
            Some(process_start_microseconds),
            Some(process_group),
            Some(terminal_process_group),
        ) => {
            terminal_tty_device == live.terminal_tty_device
                && process_start_seconds == live.process_start_seconds
                && process_start_microseconds == live.process_start_microseconds
                && process_group == live.process_group
                && terminal_process_group == live.terminal_process_group
        }
        _ => false,
    }
}

#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn verified_terminal_owner_process_group(
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live.pid {
        bail!("native-session process group no longer belongs to the recorded owner")
    }
    match (owner.process_group, owner.terminal_process_group) {
        (Some(process_group), Some(terminal_process_group)) => {
            if process_group != live.process_group
                || terminal_process_group != live.terminal_process_group
            {
                bail!("native-session process group identity changed")
            }
        }
        (None, None) => {}
        _ => bail!("native-session owner has an incomplete process group identity"),
    }
    let process_group = live.process_group;
    let terminal_process_group = live.terminal_process_group;
    if process_group != owner.pid || terminal_process_group != process_group {
        bail!("native-session owner does not lead the terminal foreground process group")
    }
    Ok(process_group)
}

#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn verified_terminal_shell_process_group(
    owner: &NativeSessionOwner,
    live_owner: &NativeProcessIdentity,
    live_shell: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live_owner.pid || live_owner.parent_pid != live_shell.pid {
        bail!("Terminal.app shell no longer owns the native-session process")
    }
    if live_shell.terminal_tty_device != live_owner.terminal_tty_device {
        bail!("Terminal.app shell is attached to a different TTY")
    }
    if live_shell.process_group != live_shell.pid
        || live_shell.terminal_process_group != live_owner.process_group
        || live_shell.process_group == live_owner.process_group
    {
        bail!("Terminal.app shell does not own the expected foreground job")
    }
    if let Some(recorded) = &owner.terminal_shell
        && (recorded.pid != live_shell.pid
            || recorded.process_group != live_shell.process_group
            || recorded.terminal_tty_device != live_shell.terminal_tty_device
            || recorded.process_start_seconds != live_shell.process_start_seconds
            || recorded.process_start_microseconds != live_shell.process_start_microseconds)
    {
        bail!("Terminal.app shell identity changed")
    }
    Ok(live_shell.process_group)
}
