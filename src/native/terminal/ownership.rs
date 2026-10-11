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
    verify_terminal_close_authority_with_observers(
        directory,
        expected_session_id,
        session,
        surface_present,
        process_birth,
        terminal_instances,
        CloseObservers {
            #[cfg(any(target_os = "macos", windows))]
            owner_record: &observe_owner_record,
            #[cfg(target_os = "macos")]
            pid_alive: &process_is_alive,
            #[cfg(target_os = "macos")]
            mac_owner_live: &mac_native_owner_is_live,
            #[cfg(target_os = "macos")]
            resumable: &|| {
                session::close::terminal_close_resumable(
                    &Reader::open_unchecked(directory),
                    session,
                    retained_surface_outlives_owner(session),
                )
            },
            ownership_proof: &|| {
                verify_terminal_surface_ownership(directory, expected_session_id, session)
            },
        },
    )
}

// Only the decision's observations are replaceable. Record reads and their ordering
// stay in the decision; production callbacks retain the existing, distinct proofs.
struct CloseObservers<'a> {
    #[cfg(any(target_os = "macos", windows))]
    owner_record: &'a dyn Fn(&NativeSessionOwner) -> crate::native::session::OwnerObservation,
    #[cfg(target_os = "macos")]
    pid_alive: &'a dyn Fn(u32) -> bool,
    #[cfg(target_os = "macos")]
    mac_owner_live: &'a dyn Fn(&NativeSessionOwner) -> Result<bool>,
    #[cfg(target_os = "macos")]
    resumable: &'a dyn Fn() -> Result<bool>,
    ownership_proof: &'a dyn Fn() -> Result<()>,
}

fn verify_terminal_close_authority_with_observers(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_present: impl FnOnce() -> Result<bool>,
    process_birth: impl Fn(u32) -> Result<Option<(u64, u64)>>,
    terminal_instances: impl Fn() -> Result<Vec<MacTerminalAppIdentity>>,
    observers: CloseObservers<'_>,
) -> Result<TerminalCloseAuthority> {
    let _ = (
        directory,
        expected_session_id,
        session,
        &surface_present,
        &process_birth,
        &terminal_instances,
    );
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
                let observed = (observers.owner_record)(&owner);
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
            if (observers.resumable)()? {
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
                if !(observers.mac_owner_live)(&owner)? {
                    session.verify_managed_session(expected_session_id)?;
                    // A reused PID grants neither mutation nor absence authority. The
                    // original owner's complete binding is required even for a query.
                    if (observers.pid_alive)(owner.pid)
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
            (observers.ownership_proof)()?;
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
        (observers.ownership_proof)()?;
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

#[cfg(all(test, target_os = "macos"))]
mod close_authority_tests {
    use super::*;
    use crate::native::session::OwnerObservation;
    use std::cell::RefCell;
    use terminal::TerminalKind::*;

    const ID: &str = "session-close-table";
    const DEAD_IDENTITY: &str = "identity is incomplete, foreign, or its PID is reused";
    const DEAD_PRESENT: &str = "visible terminal cleanup is unverified";

    fn app() -> MacTerminalAppIdentity {
        MacTerminalAppIdentity {
            pid: 456,
            start_seconds: 100,
            start_microseconds: 42,
        }
    }

    struct Row {
        kind: terminal::TerminalKind,
        owner: Option<NativeSessionOwner>,
        state: SessionState,
        phase: Option<launch::Phase>,
        intent: bool,
        surface_binding: Option<String>,
        observation_error: Option<(&'static str, usize)>,
        observed_alive: Option<bool>,
        observed_matches: Option<bool>,
        pid_alive: bool,
        live: bool,
        proof_error: bool,
        present: bool,
        birth: Option<(u64, u64)>,
        instances: Vec<MacTerminalAppIdentity>,
    }

    impl Row {
        fn new(kind: terminal::TerminalKind) -> Self {
            Self {
                kind,
                owner: Some(NativeSessionOwner {
                    pid: 123,
                    managed_session_id: Some(ID.into()),
                    terminal_tty: Some("/dev/ttys999".into()),
                    terminal_tty_device: Some(7),
                    process_start_seconds: Some(90),
                    process_start_microseconds: Some(42),
                    process_group: Some(123),
                    terminal_process_group: Some(123),
                    terminal_app: (kind == AppleTerminal).then(app),
                    ..NativeSessionOwner::default()
                }),
                state: SessionState::Ready,
                phase: None,
                intent: false,
                surface_binding: Some(ID.into()),
                observation_error: None,
                observed_alive: Some(false),
                observed_matches: None,
                pid_alive: false,
                live: false,
                proof_error: false,
                present: false,
                birth: Some((100, 42)),
                instances: vec![app()],
            }
        }

        fn check(
            self,
            expected: std::result::Result<TerminalCloseAuthority, &str>,
            calls: &[&str],
        ) {
            self.check_with_error_chain(expected, calls, None);
        }

        fn check_error_chain(self, chain: &[&str], calls: &[&str]) {
            self.check_with_error_chain(Err(""), calls, Some(chain));
        }

        fn check_with_error_chain(
            self,
            expected: std::result::Result<TerminalCloseAuthority, &str>,
            calls: &[&str],
            error_chain: Option<&[&str]>,
        ) {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open_unchecked(directory.path());
            let handle = TerminalSession {
                kind: self.kind,
                id: "surface-id".into(),
                tab_id: None,
                window_id: Some("1001".into()),
                managed_session_id: self.surface_binding,
                windows_process_identity: None,
                wezterm_mux: None,
            };
            store
                .record(CoreRecord::Terminal)
                .write_json(&handle)
                .unwrap();
            store
                .record(CoreRecord::Status)
                .write_json(&SessionStatus {
                    state: self.state,
                    generation: 0,
                    updated_unix_ms: 1,
                    exit_code: None,
                    error: None,
                    residual_surface: None,
                })
                .unwrap();
            if let Some(owner) = &self.owner {
                store.record(CoreRecord::Owner).write_json(owner).unwrap();
                if self.intent {
                    session::close::record_terminal_close_intent(&store, ID, &handle, owner)
                        .unwrap();
                }
            }
            if let Some(phase) = self.phase {
                store
                    .record(CoreRecord::Launch)
                    .write_json(&launch::Record {
                        schema: 1,
                        claim_token: "claim-table".into(),
                        deadline_unix_ms: 1,
                        phase,
                    })
                    .unwrap();
            }
            // Every row also pins the read-only boundary, including proof failures (#101).
            let snapshot = || {
                let mut records: Vec<_> = std::fs::read_dir(directory.path())
                    .unwrap()
                    .map(|entry| {
                        let path = entry.unwrap().path();
                        (
                            path.file_name().unwrap().to_owned(),
                            std::fs::read(path).unwrap(),
                        )
                    })
                    .collect();
                records.sort();
                records
            };
            let before = snapshot();
            let trace = RefCell::new(Vec::new());
            let record = |call| trace.borrow_mut().push(call);
            let observe = |call| -> Result<()> {
                record(call);
                let occurrence = trace.borrow().iter().filter(|seen| **seen == call).count();
                if self.observation_error == Some((call, occurrence)) {
                    return Err(anyhow::anyhow!("injected {call} cause"))
                        .with_context(|| format!("injected {call} failure"));
                }
                Ok(())
            };
            let handle: TerminalSession = serde_json::from_str(
                &Reader::open_unchecked(directory.path())
                    .record(CoreRecord::Terminal)
                    .text()
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            let result = verify_terminal_close_authority_with_observers(
                directory.path(),
                ID,
                &handle,
                || {
                    observe("presence")?;
                    Ok(self.present)
                },
                |pid| {
                    assert_eq!(pid, app().pid);
                    observe("birth")?;
                    Ok(self.birth)
                },
                || {
                    observe("instances")?;
                    Ok(self.instances.clone())
                },
                CloseObservers {
                    owner_record: &|owner| {
                        assert_eq!(owner.pid, 123);
                        record("owner-record");
                        OwnerObservation {
                            process_alive: self.observed_alive,
                            identity_matches: self.observed_matches,
                            error: None,
                        }
                    },
                    pid_alive: &|pid| {
                        assert_eq!(pid, 123);
                        record("pid-alive");
                        self.pid_alive
                    },
                    mac_owner_live: &|owner| {
                        assert_eq!(owner.pid, 123);
                        observe("mac-live")?;
                        Ok(self.live)
                    },
                    // Read the real intent and owner binding, substituting only its final
                    // process observation. Do not let a test boolean invent an intent.
                    resumable: &|| {
                        observe("resumable")?;
                        Ok(session::close::terminal_close_intent_owner(
                            &Reader::open_unchecked(directory.path()),
                            &handle,
                            retained_surface_outlives_owner(&handle),
                        )?
                        .is_some_and(|owner| {
                            assert_eq!(owner.pid, 123);
                            record("resume-pid-alive");
                            !self.pid_alive
                        }))
                    },
                    ownership_proof: &|| {
                        record("proof");
                        if self.proof_error {
                            bail!("injected full ownership proof failure")
                        }
                        Ok(())
                    },
                },
            );
            assert_eq!(*trace.borrow(), calls, "{:?}: {result:?}", self.kind);
            if let Some(chain) = error_chain {
                let error = result.as_ref().unwrap_err();
                assert_eq!(
                    error.chain().map(ToString::to_string).collect::<Vec<_>>(),
                    chain,
                    "{:?}: error chain changed",
                    self.kind
                );
            }
            match expected {
                Ok(authority) => assert_eq!(result.unwrap(), authority, "{:?}", self.kind),
                Err(message) => assert!(
                    format!("{:#}", result.unwrap_err()).contains(message),
                    "{:?}: expected {message}",
                    self.kind
                ),
            }
            assert_eq!(snapshot(), before, "the decision changed session records");
        }
    }

    #[test]
    fn failed_start_native_identity_dead_or_mismatched_owner_surface_only() {
        for kind in [Iterm2, Ghostty] {
            for (alive, matches) in [(Some(false), None), (Some(true), Some(false))] {
                let mut row = Row::new(kind);
                row.state = SessionState::Failed;
                row.phase = Some(launch::Phase::Spawning);
                row.observed_alive = alive;
                row.observed_matches = matches;
                row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &["owner-record"]);
            }
        }
    }

    #[test]
    fn failed_start_live_matching_or_unknown_owner_gains_no_shortcut() {
        for kind in [Iterm2, Ghostty] {
            for matches in [Some(true), None] {
                let mut row = Row::new(kind);
                row.state = SessionState::Failed;
                row.phase = Some(launch::Phase::Pending);
                row.observed_alive = Some(true);
                row.observed_matches = matches;
                row.live = true;
                row.proof_error = true;
                row.check(
                    Err("injected full ownership proof failure"),
                    &["owner-record", "resumable", "mac-live", "proof"],
                );
            }
        }
    }

    #[test]
    fn failed_start_shortcut_requires_failed_bound_owner_and_unspawned_launch() {
        for case in ["ready", "foreign", "spawned"] {
            let mut row = Row::new(Iterm2);
            row.phase = Some(launch::Phase::Spawning);
            row.state = SessionState::Failed;
            match case {
                "ready" => row.state = SessionState::Ready,
                "foreign" => {
                    row.owner.as_mut().unwrap().managed_session_id = Some("foreign".into())
                }
                _ => row.phase = Some(launch::Phase::Spawned),
            }
            let mut calls = vec![];
            if case != "spawned" {
                calls.push("owner-record");
            }
            calls.extend(["resumable", "mac-live", "pid-alive"]);
            if case == "foreign" {
                row.check(Err(DEAD_IDENTITY), &calls);
            } else {
                calls.push("presence");
                row.check(Ok(TerminalCloseAuthority::Absent), &calls);
            }
        }
    }

    #[test]
    fn resumable_close_intent_native_surfaces_surface_only() {
        for kind in [Warp, WezTerm, Ghostty] {
            let mut row = Row::new(kind);
            row.intent = true;
            row.check(
                Ok(TerminalCloseAuthority::SurfaceOnly),
                &["resumable", "resume-pid-alive"],
            );
        }
    }

    #[test]
    fn close_intent_live_pid_does_not_resume() {
        let mut row = Row::new(Ghostty);
        row.intent = true;
        row.pid_alive = true;
        row.live = true;
        row.check(
            Ok(TerminalCloseAuthority::LiveOwner),
            &["resumable", "resume-pid-alive", "mac-live", "proof"],
        );
    }

    #[test]
    fn resumable_terminal_recorded_incarnation_surface_only_absent_or_error() {
        for case in ["live", "ended", "reused", "ambiguous"] {
            let mut row = Row::new(AppleTerminal);
            row.intent = true;
            let mut calls = vec!["resumable", "resume-pid-alive", "birth"];
            let expected = match case {
                "ended" => {
                    row.birth = None;
                    Ok(TerminalCloseAuthority::Absent)
                }
                "reused" => {
                    row.birth = Some((101, 42));
                    Err("PID was reused")
                }
                "ambiguous" => {
                    row.instances.clear();
                    calls.push("instances");
                    Err("scripting target is ambiguous or changed")
                }
                _ => {
                    calls.push("instances");
                    Ok(TerminalCloseAuthority::SurfaceOnly)
                }
            };
            row.check(expected, &calls);
        }
    }

    #[test]
    fn resumable_terminal_without_incarnation_absent_or_error() {
        for present in [false, true] {
            let mut row = Row::new(AppleTerminal);
            row.intent = true;
            row.owner.as_mut().unwrap().terminal_app = None;
            row.present = present;
            row.check(
                if present {
                    Err("process incarnation was not recorded")
                } else {
                    Ok(TerminalCloseAuthority::Absent)
                },
                &[
                    "resumable",
                    "resume-pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            );
        }
    }

    #[test]
    fn dead_owner_incomplete_foreign_or_reused_pid_refused_before_surface() {
        for case in [
            "incomplete",
            "foreign",
            "reused",
            "missing-tty",
            "empty-tty",
        ] {
            let mut row = Row::new(Iterm2);
            let owner = row.owner.as_mut().unwrap();
            match case {
                "incomplete" => owner.process_group = None,
                "foreign" => owner.managed_session_id = Some("foreign".into()),
                "reused" => row.pid_alive = true,
                "missing-tty" => owner.terminal_tty = None,
                _ => owner.terminal_tty = Some(String::new()),
            }
            row.check(Err(DEAD_IDENTITY), &["resumable", "mac-live", "pid-alive"]);
        }
    }

    #[test]
    fn dead_owner_surface_absent_or_present() {
        for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
            for present in [false, true] {
                let mut row = Row::new(kind);
                row.present = present;
                let mut calls = vec!["resumable", "mac-live", "pid-alive"];
                if kind == AppleTerminal {
                    calls.extend(["birth", "instances"]);
                }
                calls.push("presence");
                if kind == AppleTerminal {
                    calls.extend(["birth", "instances"]);
                }
                row.check(
                    if present {
                        Err(DEAD_PRESENT)
                    } else {
                        Ok(TerminalCloseAuthority::Absent)
                    },
                    &calls,
                );
            }
        }
    }

    #[test]
    fn dead_terminal_owner_ended_incarnation_absent_without_surface_query() {
        let mut row = Row::new(AppleTerminal);
        row.birth = None;
        row.check(
            Ok(TerminalCloseAuthority::Absent),
            &["resumable", "mac-live", "pid-alive", "birth"],
        );
    }

    #[test]
    fn live_owner_full_proof_success_or_original_error() {
        for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
            for proof_error in [false, true] {
                let mut row = Row::new(kind);
                row.live = true;
                row.proof_error = proof_error;
                let mut calls = vec!["resumable", "mac-live"];
                if kind == AppleTerminal {
                    calls.extend(["birth", "instances"]);
                }
                calls.push("proof");
                row.check(
                    if proof_error {
                        Err("injected full ownership proof failure")
                    } else {
                        Ok(TerminalCloseAuthority::LiveOwner)
                    },
                    &calls,
                );
            }
        }
    }

    #[test]
    fn live_terminal_owner_ended_incarnation_refused_before_proof() {
        let mut row = Row::new(AppleTerminal);
        row.live = true;
        row.birth = None;
        row.check(
            Err("Terminal.app ended while its native owner remained"),
            &["resumable", "mac-live", "birth"],
        );
    }

    #[test]
    fn ownerless_non_startup_status_refused_without_observation() {
        for state in [
            SessionState::Ready,
            SessionState::Working,
            SessionState::Closed,
        ] {
            let mut row = Row::new(Iterm2);
            row.owner = None;
            row.state = state;
            row.check(
                Err("terminal close requires a live native-session owner"),
                &[],
            );
        }
    }

    #[test]
    fn ownerless_terminal_startup_absent_or_present() {
        for state in [SessionState::Launching, SessionState::Failed] {
            for present in [false, true] {
                let mut row = Row::new(AppleTerminal);
                row.owner = None;
                row.state = state.clone();
                row.present = present;
                row.check(
                    if present {
                        Err("startup has no native owner/app incarnation")
                    } else {
                        Ok(TerminalCloseAuthority::Absent)
                    },
                    &["instances", "presence", "instances"],
                );
            }
        }
    }

    #[test]
    fn ownerless_warp_wezterm_startup_absent_or_present() {
        for kind in [Warp, WezTerm] {
            for state in [SessionState::Launching, SessionState::Failed] {
                for present in [false, true] {
                    let mut row = Row::new(kind);
                    row.owner = None;
                    row.state = state.clone();
                    row.present = present;
                    row.check(
                        if present {
                            Err("startup has no native owner or close intent")
                        } else {
                            Ok(TerminalCloseAuthority::Absent)
                        },
                        &["presence"],
                    );
                }
            }
        }
    }

    #[test]
    fn ownerless_native_close_identity_startup_surface_only() {
        for kind in [Iterm2, Ghostty] {
            for state in [SessionState::Launching, SessionState::Failed] {
                let mut row = Row::new(kind);
                row.owner = None;
                row.state = state;
                row.check(Ok(TerminalCloseAuthority::SurfaceOnly), &[]);
            }
        }
    }

    #[test]
    fn resumability_error_stops_before_owner_liveness() {
        let mut row = Row::new(Ghostty);
        row.intent = true;
        row.observation_error = Some(("resumable", 1));
        row.check_error_chain(
            &["injected resumable failure", "injected resumable cause"],
            &["resumable"],
        );
    }

    #[test]
    fn mac_owner_liveness_error_stops_before_pid_surface_and_proof() {
        let mut row = Row::new(AppleTerminal);
        row.observation_error = Some(("mac-live", 1));
        row.check_error_chain(
            &["injected mac-live failure", "injected mac-live cause"],
            &["resumable", "mac-live"],
        );
    }

    #[test]
    fn dead_owner_presence_error_never_grants_absence() {
        for kind in [Iterm2, Ghostty, Warp, WezTerm, AppleTerminal] {
            let mut row = Row::new(kind);
            row.observation_error = Some(("presence", 1));
            let mut calls = vec!["resumable", "mac-live", "pid-alive"];
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("presence");
            row.check_error_chain(
                &[
                    "could not prove absence of the dead owner's exact terminal surface; no close was sent",
                    "injected presence failure",
                    "injected presence cause",
                ],
                &calls,
            );
        }
    }

    #[test]
    fn ownerless_startup_presence_error_never_grants_absence() {
        for kind in [Warp, WezTerm, AppleTerminal] {
            let mut row = Row::new(kind);
            row.owner = None;
            row.state = SessionState::Launching;
            row.observation_error = Some(("presence", 1));
            let mut chain = vec![];
            let mut calls = vec![];
            if kind == AppleTerminal {
                calls.push("instances");
            } else {
                chain.push(
                    "could not prove absence of the ownerless startup surface; no close was sent",
                );
            }
            chain.extend(["injected presence failure", "injected presence cause"]);
            calls.push("presence");
            row.check_error_chain(&chain, &calls);
        }
    }

    #[test]
    fn terminal_process_birth_errors_preserve_chain_and_stop_at_observation() {
        for (path, occurrence, calls) in [
            (
                "resumable",
                1,
                vec!["resumable", "resume-pid-alive", "birth"],
            ),
            ("live", 1, vec!["resumable", "mac-live", "birth"]),
            (
                "dead",
                1,
                vec!["resumable", "mac-live", "pid-alive", "birth"],
            ),
            (
                "dead",
                2,
                vec![
                    "resumable",
                    "mac-live",
                    "pid-alive",
                    "birth",
                    "instances",
                    "presence",
                    "birth",
                ],
            ),
        ] {
            let mut row = Row::new(AppleTerminal);
            row.intent = path == "resumable";
            row.live = path == "live";
            row.observation_error = Some(("birth", occurrence));
            let mut chain = vec![];
            if path == "dead" {
                chain.push("could not prove absence of the dead owner's exact terminal surface; no close was sent");
            }
            chain.extend(["injected birth failure", "injected birth cause"]);
            row.check_error_chain(&chain, &calls);
        }
    }

    #[test]
    fn terminal_instance_errors_preserve_chain_and_stop_at_observation() {
        for (path, recorded, occurrence, calls) in [
            (
                "resumable",
                true,
                1,
                vec!["resumable", "resume-pid-alive", "birth", "instances"],
            ),
            (
                "resumable",
                false,
                1,
                vec!["resumable", "resume-pid-alive", "instances"],
            ),
            (
                "resumable",
                false,
                2,
                vec![
                    "resumable",
                    "resume-pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
            (
                "live",
                true,
                1,
                vec!["resumable", "mac-live", "birth", "instances"],
            ),
            (
                "dead",
                true,
                1,
                vec!["resumable", "mac-live", "pid-alive", "birth", "instances"],
            ),
            (
                "dead",
                true,
                2,
                vec![
                    "resumable",
                    "mac-live",
                    "pid-alive",
                    "birth",
                    "instances",
                    "presence",
                    "birth",
                    "instances",
                ],
            ),
            (
                "dead",
                false,
                1,
                vec!["resumable", "mac-live", "pid-alive", "instances"],
            ),
            (
                "dead",
                false,
                2,
                vec![
                    "resumable",
                    "mac-live",
                    "pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            ),
            ("startup", false, 1, vec!["instances"]),
            (
                "startup",
                false,
                2,
                vec!["instances", "presence", "instances"],
            ),
        ] {
            let mut row = Row::new(AppleTerminal);
            row.intent = path == "resumable";
            row.live = path == "live";
            if !recorded {
                row.owner.as_mut().unwrap().terminal_app = None;
            }
            if path == "startup" {
                row.owner = None;
                row.state = SessionState::Launching;
            }
            row.observation_error = Some(("instances", occurrence));
            let mut chain = vec![];
            if path == "dead" {
                chain.push("could not prove absence of the dead owner's exact terminal surface; no close was sent");
            }
            chain.extend(["injected instances failure", "injected instances cause"]);
            row.check_error_chain(&chain, &calls);
        }
    }

    #[test]
    fn failed_unspawned_terminal_warp_wezterm_skip_native_identity_recovery() {
        for kind in [AppleTerminal, Warp, WezTerm] {
            let mut row = Row::new(kind);
            row.state = SessionState::Failed;
            row.phase = Some(launch::Phase::Spawning);
            row.present = true;
            let mut calls = vec!["resumable", "mac-live", "pid-alive"];
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            calls.push("presence");
            if kind == AppleTerminal {
                calls.extend(["birth", "instances"]);
            }
            row.check(Err(DEAD_PRESENT), &calls);
        }
    }

    #[test]
    fn iterm_recorded_close_intent_does_not_resume() {
        let mut row = Row::new(Iterm2);
        row.intent = true;
        row.present = true;
        row.check(
            Err(DEAD_PRESENT),
            &["resumable", "mac-live", "pid-alive", "presence"],
        );
    }

    #[test]
    fn live_terminal_owner_without_incarnation_proceeds_to_proof() {
        let mut row = Row::new(AppleTerminal);
        row.owner.as_mut().unwrap().terminal_app = None;
        row.live = true;
        row.check(
            Ok(TerminalCloseAuthority::LiveOwner),
            &["resumable", "mac-live", "proof"],
        );
    }

    #[test]
    fn live_terminal_owner_reused_or_ambiguous_incarnation_refuses_before_proof() {
        for reused in [true, false] {
            let mut row = Row::new(AppleTerminal);
            row.live = true;
            if reused {
                row.birth = Some((101, 42));
                row.check_error_chain(
                    &["Terminal.app PID was reused; no close or absence authority was granted"],
                    &["resumable", "mac-live", "birth"],
                );
            } else {
                row.instances
                    .push(MacTerminalAppIdentity { pid: 789, ..app() });
                row.check_error_chain(
                    &["Terminal.app scripting target is ambiguous or changed; the recorded app must be the only running instance and the exact surface handle is retained"],
                    &["resumable", "mac-live", "birth", "instances"],
                );
            }
        }
    }

    #[test]
    fn dead_terminal_owner_without_incarnation_requires_stable_absence() {
        for present in [false, true] {
            let mut row = Row::new(AppleTerminal);
            row.owner.as_mut().unwrap().terminal_app = None;
            row.present = present;
            row.check(
                if present {
                    Err(DEAD_PRESENT)
                } else {
                    Ok(TerminalCloseAuthority::Absent)
                },
                &[
                    "resumable",
                    "mac-live",
                    "pid-alive",
                    "instances",
                    "presence",
                    "instances",
                ],
            );
        }
    }

    #[test]
    fn foreign_or_missing_surface_binding_refuses_before_surface_observation() {
        for path in ["failed-start", "dead", "intent", "startup"] {
            for binding in [Some("foreign"), None] {
                let mut row = Row::new(Ghostty);
                row.surface_binding = binding.map(str::to_owned);
                let calls = match path {
                    "failed-start" => {
                        row.state = SessionState::Failed;
                        row.phase = Some(launch::Phase::Spawning);
                        vec!["owner-record"]
                    }
                    "startup" => {
                        row.owner = None;
                        row.state = SessionState::Launching;
                        vec![]
                    }
                    _ => {
                        row.intent = path == "intent";
                        vec!["resumable", "mac-live"]
                    }
                };
                row.check_error_chain(
                    &[if binding.is_some() {
                        "terminal handle belongs to managed session foreign, not session-close-table"
                    } else {
                        "terminal handle is missing its managed session binding"
                    }],
                    &calls,
                );
            }
        }
    }

    // There is no current TerminalKind that reaches "no supported native close
    // identity" on macOS: AppleTerminal/Warp/WezTerm return earlier and every
    // remaining kind (including WindowsConsole) has native close identity.
}
