use super as terminal;
#[cfg(target_os = "macos")]
use super::macos::apple_terminal::{
    require_unique_terminal_app, terminal_app_alive_with, terminal_app_instances,
    terminal_app_process, terminal_surface_absent, terminate_apple_terminal_owner,
};
#[cfg(target_os = "macos")]
pub(in crate::native) use super::macos::ownership::mac_native_owner_is_live;
#[cfg(target_os = "macos")]
use super::macos::ownership::verified_macos_terminal_owner;
#[cfg(target_os = "macos")]
use super::macos::process::macos_process_start;
#[cfg(target_os = "macos")]
use super::macos::process::{
    current_terminal_tty, live_native_process_identity, terminal_tty_device,
};
#[cfg(target_os = "macos")]
use super::macos::warp::{prepare_warp_close, terminate_owned_foreground_group};
#[cfg(windows)]
pub(in crate::native) use super::windows::ownership::verified_windows_native_owner;
use super::{CloseOutcome, TerminalSession};
#[cfg(any(target_os = "macos", windows))]
use crate::native::SessionStatus;
#[cfg(target_os = "macos")]
use crate::native::session::{self, Store};
#[cfg(any(target_os = "macos", windows))]
use crate::native::session::{CoreRecord, Reader, SessionState, launch, observe_owner_record};
use agent_bridge::process_is_alive;
#[cfg(any(target_os = "macos", test))]
use anyhow::Context;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

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

pub(in crate::native) fn verify_terminal_surface_ownership(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    verify_terminal_surface_ownership_with_timeout(directory, expected_session_id, session, None)
}

pub(in crate::native) fn verify_terminal_surface_ownership_with_timeout(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    session.verify_managed_session(expected_session_id)?;
    #[cfg(windows)]
    {
        let _ = timeout;
        verified_windows_native_owner(directory, expected_session_id)?;
    }
    #[cfg(target_os = "macos")]
    {
        let surface_tty = terminal::verify_macos_surface(session, timeout)?;
        verified_macos_terminal_owner(
            directory,
            expected_session_id,
            session,
            surface_tty.as_deref(),
        )?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (directory, timeout);
    Ok(())
}

// The macOS surfaces that can outlive their native owner: Terminal.app keeps the window
// of an ended shell, and Warp, WezTerm and Ghostty surfaces can remain. Outside the
// native-ID failed-start recovery below, only the close that recorded its intent first
// may reach a surface after its owner ended. Otherwise only proven absence consumes the
// handle; repair itself never closes it.
#[cfg(target_os = "macos")]
pub(in crate::native) fn surface_outlives_owner(kind: terminal::TerminalKind) -> bool {
    matches!(
        kind,
        terminal::TerminalKind::AppleTerminal
            | terminal::TerminalKind::Warp
            | terminal::TerminalKind::WezTerm
            | terminal::TerminalKind::Ghostty
    )
}

#[cfg(any(target_os = "macos", windows))]
fn failed_start_has_native_close_identity(kind: terminal::TerminalKind) -> bool {
    matches!(
        kind,
        terminal::TerminalKind::Iterm2
            | terminal::TerminalKind::Ghostty
            | terminal::TerminalKind::WindowsConsole
    )
}

pub(in crate::native) fn verify_terminal_close_authority(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<TerminalCloseAuthority> {
    verify_terminal_close_authority_with_presence(directory, expected_session_id, session, || {
        terminal::surface_present(session, Duration::from_secs(3))
    })
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::native) enum TerminalCloseAuthority {
    LiveOwner,
    #[cfg(any(target_os = "macos", windows))]
    SurfaceOnly,
    Absent,
}

fn verify_terminal_close_authority_with_presence(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_present: impl FnOnce() -> Result<bool>,
) -> Result<TerminalCloseAuthority> {
    verify_terminal_close_authority_with_observations(
        directory,
        expected_session_id,
        session,
        surface_present,
        |pid| {
            #[cfg(target_os = "macos")]
            {
                macos_process_start(pid)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = pid;
                bail!("macOS app observation is unavailable on this platform")
            }
        },
        || {
            #[cfg(target_os = "macos")]
            {
                terminal_app_instances()
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("macOS app observation is unavailable on this platform")
            }
        },
    )
}

pub(in crate::native) fn verify_terminal_close_authority_with_observations(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_present: impl FnOnce() -> Result<bool>,
    process_birth: impl Fn(u32) -> Result<Option<(u64, u64)>>,
    terminal_instances: impl Fn() -> Result<Vec<MacTerminalAppIdentity>>,
) -> Result<TerminalCloseAuthority> {
    let _ = (&surface_present, &process_birth, &terminal_instances);
    #[cfg(any(target_os = "macos", windows))]
    {
        if Reader::open_unchecked(directory)
            .record(CoreRecord::Owner)
            .text()?
            .is_some()
        {
            // An explicit close may reclaim the exact startup surface after its wrapper
            // died, including an uncertain spawn. This is not automatic failure cleanup:
            // neither a live/unknown owner nor an unbound/foreign handle gains authority.
            // Terminal.app, Warp and WezTerm gain none here: a failed launch is no close
            // intent, and the dead-owner rules below apply to them.
            if let Some(launch) = launch::read(&Reader::open_unchecked(directory))?
                && launch.phase != launch::Phase::Spawned
                && failed_start_has_native_close_identity(session.kind)
            {
                let status: SessionStatus = Reader::open_unchecked(directory).status()?;
                let owner: NativeSessionOwner = Reader::open_unchecked(directory).owner()?;
                let observed = observe_owner_record(&owner);
                if status.state == SessionState::Failed
                    && owner.managed_session_id.as_deref() == Some(expected_session_id)
                    && (observed.process_alive == Some(false)
                        || observed.identity_matches == Some(false))
                {
                    session.verify_managed_session(expected_session_id)?;
                    return Ok(TerminalCloseAuthority::SurfaceOnly);
                }
            }
            // The close that recorded the intent may finish without signalling anything
            // again, and only once the owner it verified no longer exists. A live owner,
            // or a PID that is alive again, keeps the rules below.
            #[cfg(target_os = "macos")]
            if session::close::terminal_close_resumable(
                &Reader::open_unchecked(directory),
                session,
                retained_surface_outlives_owner(session),
            )? {
                session.verify_managed_session(expected_session_id)?;
                if session.kind == terminal::TerminalKind::AppleTerminal {
                    let owner: NativeSessionOwner = Reader::open_unchecked(directory).owner()?;
                    let Some(app) = &owner.terminal_app else {
                        // An intent of 0.0.10 or earlier names no app incarnation, and
                        // its owner is gone: nothing can tie the window to an app now.
                        if terminal_surface_absent(
                            None,
                            &process_birth,
                            &terminal_instances,
                            surface_present,
                        )? {
                            return Ok(TerminalCloseAuthority::Absent);
                        }
                        bail!(
                            "Terminal.app process incarnation was not recorded and its window is not proven gone; no close was sent and the exact surface handle is retained"
                        );
                    };
                    if !terminal_app_alive_with(app, &process_birth)? {
                        return Ok(TerminalCloseAuthority::Absent);
                    }
                    require_unique_terminal_app(app, &terminal_instances()?)?;
                }
                return Ok(TerminalCloseAuthority::SurfaceOnly);
            }
            #[cfg(target_os = "macos")]
            {
                let owner: NativeSessionOwner = Reader::open_unchecked(directory).owner()?;
                if !mac_native_owner_is_live(&owner)? {
                    session.verify_managed_session(expected_session_id)?;
                    // A reused PID grants neither mutation nor absence authority. The
                    // original owner's complete binding is required even for a query.
                    if process_is_alive(owner.pid)
                        || owner.managed_session_id.as_deref() != Some(expected_session_id)
                        || !matches!(
                            (
                                owner.terminal_tty_device,
                                owner.process_start_seconds,
                                owner.process_start_microseconds,
                                owner.process_group,
                                owner.terminal_process_group
                            ),
                            (Some(_), Some(_), Some(_), Some(_), Some(_))
                        )
                        || owner.terminal_tty.as_deref().is_none_or(str::is_empty)
                    {
                        bail!(
                            "dead native-session owner identity is incomplete, foreign, or its PID is reused; no terminal observation or close was sent"
                        );
                    }
                    let absent = if session.kind == terminal::TerminalKind::AppleTerminal {
                        terminal_surface_absent(
                            owner.terminal_app.as_ref(),
                            &process_birth,
                            &terminal_instances,
                            surface_present,
                        )
                    } else {
                        surface_present().map(|present| !present)
                    }
                    .context("could not prove absence of the dead owner's exact terminal surface; no close was sent")?;
                    if absent {
                        return Ok(TerminalCloseAuthority::Absent);
                    }
                    bail!(
                        "the recorded native-session owner is no longer live; visible terminal cleanup is unverified and no close was sent"
                    );
                }
                // A live owner of 0.0.10 or earlier names no app incarnation: the close
                // derives it from that owner's own ancestry before it records its intent.
                if session.kind == terminal::TerminalKind::AppleTerminal
                    && let Some(app) = &owner.terminal_app
                {
                    if !terminal_app_alive_with(app, &process_birth)? {
                        bail!(
                            "Terminal.app ended while its native owner remained; no close was sent"
                        );
                    }
                    require_unique_terminal_app(app, &terminal_instances()?)?;
                }
            }
            verify_terminal_surface_ownership(directory, expected_session_id, session)?;
            return Ok(TerminalCloseAuthority::LiveOwner);
        }
        let status: SessionStatus = Reader::open_unchecked(directory).status()?;
        if !matches!(status.state, SessionState::Launching | SessionState::Failed) {
            bail!(
                "terminal close requires a live native-session owner while the session is {}",
                status.state
            );
        }
        // Startup can fail after the exact surface handle is durably bound but before the
        // provider wrapper writes native-session.json. Explicit close may recover only that
        // bound launch surface; the adapter still targets its stable native identifiers.
        session.verify_managed_session(expected_session_id)?;
        // A Terminal.app window and tty are identities only inside the app incarnation
        // that no wrapper recorded here, so nothing is closed: the handle is consumed
        // once the window is proven gone.
        #[cfg(target_os = "macos")]
        if session.kind == terminal::TerminalKind::AppleTerminal {
            if terminal_surface_absent(None, &process_birth, &terminal_instances, surface_present)?
            {
                return Ok(TerminalCloseAuthority::Absent);
            }
            bail!(
                "Terminal.app startup has no native owner/app incarnation and its window is not proven gone; no close was sent and the surface handle is retained"
            );
        }
        #[cfg(target_os = "macos")]
        if matches!(
            session.kind,
            terminal::TerminalKind::Warp | terminal::TerminalKind::WezTerm
        ) {
            if !surface_present().context(
                "could not prove absence of the ownerless startup surface; no close was sent",
            )? {
                return Ok(TerminalCloseAuthority::Absent);
            }
            bail!(
                "startup has no native owner or close intent and its surface is not proven gone; no close was sent and the surface handle is retained"
            );
        }
        if !failed_start_has_native_close_identity(session.kind) {
            bail!("startup surface has no supported native close identity; its handle is retained");
        }
        Ok(TerminalCloseAuthority::SurfaceOnly)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        verify_terminal_surface_ownership(directory, expected_session_id, session)?;
        Ok(TerminalCloseAuthority::LiveOwner)
    }
}

// Platform identity checks remain outside the session records layer.
pub(in crate::native) fn owner_blocks_prune(owner: &NativeSessionOwner) -> Result<bool> {
    #[cfg(windows)]
    {
        let Some(identity) = &owner.windows_process_identity else {
            return Ok(true);
        };
        if !process_is_alive(owner.pid) {
            return Ok(false);
        }
        match terminal::windows_process_identity(owner.pid) {
            Ok(live) => Ok(&live == identity),
            Err(_) => Ok(true),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if !process_is_alive(owner.pid) {
            return Ok(false);
        }
        let (Some(seconds), Some(microseconds)) = (
            owner.process_start_seconds,
            owner.process_start_microseconds,
        ) else {
            return Ok(true);
        };
        match live_native_process_identity(owner.pid) {
            Ok(live) => Ok(live.process_start_seconds == seconds
                && live.process_start_microseconds == microseconds),
            Err(_) => Ok(true),
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Ok(process_is_alive(owner.pid))
    }
}

pub(in crate::native) fn repair_owner_is_live(owner: &NativeSessionOwner) -> Result<bool> {
    #[cfg(windows)]
    match &owner.windows_process_identity {
        Some(identity) => {
            if terminal::verify_windows_process_identity(owner.pid, identity).is_ok() {
                return Ok(true);
            }
        }
        // Pre-identity (v0.0.2) Windows owner records carry only a PID. Their identity is
        // unknown, not dead: while the PID is alive the session is left alone and inspect
        // reports `identity_matches: null`; only a dead PID lets repair proceed.
        None => {
            if process_is_alive(owner.pid) {
                return Ok(true);
            }
        }
    }
    #[cfg(target_os = "macos")]
    if mac_native_owner_is_live(owner)? {
        return Ok(true);
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    if process_is_alive(owner.pid) {
        return Ok(true);
    }
    Ok(false)
}

pub(in crate::native) fn retained_surface_outlives_owner(
    session: &terminal::TerminalSession,
) -> bool {
    #[cfg(target_os = "macos")]
    {
        surface_outlives_owner(session.kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = session;
        false
    }
}

pub(in crate::native) fn close_dead_owner_surface(
    session: &terminal::TerminalSession,
) -> Result<terminal::CloseOutcome> {
    close_dead_owner_surface_with(session, terminal::close_session)
}

pub(in crate::native) fn close_dead_owner_surface_with(
    session: &terminal::TerminalSession,
    mut closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
) -> Result<terminal::CloseOutcome> {
    #[cfg(windows)]
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("dead Windows native owner has a non-Windows terminal handle")
    }
    closer(session)
}

pub(in crate::native) fn close_owned_surface(
    directory: &Path,
    id: &str,
    session: &TerminalSession,
) -> Result<CloseOutcome> {
    let authority = verify_terminal_close_authority(directory, id, session)?;
    if authority == TerminalCloseAuthority::Absent {
        return Ok(terminal::CloseOutcome::Missing);
    }
    let has_native_owner = authority == TerminalCloseAuthority::LiveOwner;
    #[cfg(target_os = "macos")]
    if has_native_owner && session.kind == terminal::TerminalKind::AppleTerminal {
        terminate_apple_terminal_owner(directory, id, session)?;
    }
    #[cfg(target_os = "macos")]
    if has_native_owner && session.kind == terminal::TerminalKind::Warp {
        let (owner, live) = verified_macos_terminal_owner(directory, id, session, None)?;
        let shell = live_native_process_identity(live.parent_pid)?;
        prepare_warp_close(
            directory,
            id,
            session,
            &owner,
            &live,
            &shell,
            terminate_owned_foreground_group,
        )?;
    }
    #[cfg(target_os = "macos")]
    if has_native_owner && session.kind == terminal::TerminalKind::WezTerm {
        // tab.close/kill-pane can end the owner while surface cleanup still fails.
        // Preserve this exact requested close before its first external mutation.
        let (owner, _) = verified_macos_terminal_owner(directory, id, session, None)?;
        session::close::record_terminal_close_intent(
            &Store::open_unchecked(directory),
            id,
            session,
            &owner,
        )?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = has_native_owner;
    #[cfg(target_os = "macos")]
    if session.kind == terminal::TerminalKind::AppleTerminal {
        let owner: NativeSessionOwner = Reader::open_unchecked(directory).owner()?;
        return terminal::macos::apple_terminal::close_attested_session(
            session,
            owner
                .terminal_app
                .as_ref()
                .context("Terminal.app close has no app incarnation")?,
        );
    }
    terminal::close_session(session)
}

pub(in crate::native) fn current_surface_owner(
    directory: &Path,
    id: &str,
) -> Result<NativeSessionOwner> {
    #[cfg(not(target_os = "macos"))]
    let _ = directory;
    let owner = current_native_session_owner(id)?;
    #[cfg(target_os = "macos")]
    let owner = {
        let mut owner = owner;
        let surface: terminal::TerminalSession = Reader::open_unchecked(directory).terminal()?;
        surface.verify_managed_session(id)?;
        if surface.kind == terminal::TerminalKind::AppleTerminal {
            owner.terminal_app = Some(terminal_app_process(&owner)?);
        }
        owner
    };
    Ok(owner)
}
