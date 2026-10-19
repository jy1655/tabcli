use super as terminal;
#[cfg(target_os = "macos")]
use super::CloseOutcome;
#[cfg(target_os = "macos")]
use super::TerminalSession;
#[cfg(target_os = "macos")]
use super::macos::ownership::verified_macos_terminal_owner;
#[cfg(target_os = "macos")]
use super::macos::process::{
    current_terminal_tty, live_native_process_identity, terminal_tty_device,
};
#[cfg(windows)]
pub(in crate::native) use super::windows::ownership::verified_windows_native_owner;
#[cfg(all(test, target_os = "macos"))]
use crate::native::process_identity::{Gone, ProcessObservation};
#[cfg(target_os = "macos")]
use crate::native::session::Reader;
use crate::native::session::close::budget::close_remaining;
#[cfg(all(test, target_os = "macos"))]
use crate::native::session::owner::OwnerAttestation;
#[cfg(target_os = "macos")]
use crate::native::session::{self, Store};
#[cfg(any(target_os = "macos", test))]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "macos", windows, test))]
use anyhow::bail;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::time::Instant;
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

#[cfg(any(target_os = "macos", test))]
pub(in crate::native::terminal) fn verify_native_terminal_owner_with(
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
    owner: NativeSessionOwner,
    mut process: impl FnMut(u32) -> Result<NativeProcessIdentity>,
    tty_device: impl FnOnce(&Path) -> Result<u64>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let live = process(owner.pid)?;
    session.verify_managed_session(expected_session_id)?;
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    if !matches!(
        (
            owner.terminal_tty_device,
            owner.process_start_seconds,
            owner.process_start_microseconds,
            owner.process_group,
            owner.terminal_process_group,
        ),
        (Some(_), Some(_), Some(_), Some(_), Some(_))
    ) || !native_owner_identity_matches(&owner, &live)
    {
        bail!("native-session owner birth, TTY, or foreground identity changed")
    }
    let owner_tty = owner
        .terminal_tty
        .as_deref()
        .context("native-session owner is missing its controlling TTY")?;
    if surface_tty.is_some_and(|tty| tty != owner_tty) {
        bail!("terminal surface is attached to a different native-session TTY")
    }
    if tty_device(Path::new(owner_tty))? != live.terminal_tty_device {
        bail!("native-session owner TTY path no longer identifies its controlling TTY")
    }
    verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = process(live.parent_pid)?;
    verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    Ok((owner, live))
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

#[cfg(windows)]
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
        let surface_tty = terminal::verify_macos_surface(directory, session, timeout)?;
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

pub(in crate::native) fn close_dead_owner_surface_until(
    directory: &Path,
    session: &terminal::TerminalSession,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    close_dead_owner_surface_with(
        session,
        |surface| terminal::close_session_until(directory, surface, deadline),
        deadline,
    )
}

pub(in crate::native) fn close_dead_owner_surface_with(
    session: &terminal::TerminalSession,
    mut closer: impl FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseOutcome> {
    #[cfg(windows)]
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("dead Windows native owner has a non-Windows terminal handle")
    }
    close_remaining(deadline)?;
    let outcome = closer(session)?;
    close_remaining(deadline)?;
    Ok(outcome)
}

// The close decision and its dispatch live in `close_authority`; the production entry
// keeps its path here for `native.rs`.
pub(in crate::native) use super::close_authority::close_owned_surface;

#[cfg(target_os = "macos")]
const OWNER_TEARDOWN_REASON: &str =
    "the surface was proven absent while its owner was live; the owner was ended";

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn owner_group_gone(group: u32) -> Result<bool> {
    owner_group_gone_with(group, signal_process_group)
}

#[cfg(target_os = "macos")]
fn owner_group_gone_with(
    group: u32,
    mut signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
) -> Result<bool> {
    let target = process_group_signal_target(group)?;
    match signal(target, 0) {
        Ok(()) => Ok(false),
        Err(error) => match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            Some(libc::EPERM) => Ok(false),
            _ => Err(error).context("could not observe the teardown owner's process group"),
        },
    }
}

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn teardown_absent_owner_with(
    store: &Store,
    id: &str,
    surface: &TerminalSession,
    mut attest: impl FnMut() -> Result<(NativeSessionOwner, NativeProcessIdentity)>,
    stop: impl FnOnce(u32) -> Result<()>,
    deadline: std::time::Instant,
) -> Result<terminal::CloseResult> {
    close_remaining(deadline)?;
    let (owner, live) = attest()?;
    close_remaining(deadline)?;
    let group = verified_terminal_owner_process_group(&owner, &live)?;
    session::close::record_terminal_teardown_intent(
        store,
        id,
        surface,
        &owner,
        group,
        OWNER_TEARDOWN_REASON,
    )?;
    #[cfg(test)]
    session::close::interruption::at(session::close::interruption::Point::IntentPublished);
    // The second full attestation reads the owner, tty and parent shell again.
    // A replacement record, even if it attests successfully, is not this intent.
    close_remaining(deadline)?;
    let (again, current) = attest()?;
    close_remaining(deadline)?;
    if serde_json::to_value(&owner)? != serde_json::to_value(&again)?
        || !native_owner_identity_matches(&owner, &current)
        || verified_terminal_owner_process_group(&again, &current)? != group
    {
        bail!("the teardown owner changed after intent publication; no signal was sent");
    }
    close_remaining(deadline)?;
    stop(group)?;
    close_remaining(deadline)?;
    Ok(terminal::CloseResult {
        outcome: CloseOutcome::Missing,
        reason: Some(OWNER_TEARDOWN_REASON.to_owned()),
    })
}

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn process_group_signal_target(
    process_group: u32,
) -> Result<libc::pid_t> {
    let process_group = libc::pid_t::try_from(process_group)
        .context("Terminal.app process group is out of range")?;
    if process_group <= 0 {
        bail!("Terminal.app process group must be positive")
    }
    process_group
        .checked_neg()
        .context("Terminal.app process group cannot be represented as a signal target")
}

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn terminate_owned_foreground_group_until(
    group: u32,
    deadline: Instant,
) -> Result<()> {
    close_remaining(deadline)?;
    terminate_owned_foreground_group_until_with_io(
        group,
        deadline.min(Instant::now() + Duration::from_secs(3)),
        signal_process_group,
        |_| {},
    )
}

#[cfg(all(test, target_os = "macos"))]
pub(in crate::native) fn terminate_owned_foreground_group(group: u32) -> Result<()> {
    terminate_owned_foreground_group_with(group, Duration::from_secs(3), |_| {})
}

#[cfg(all(test, target_os = "macos"))]
fn terminate_owned_foreground_group_with(
    group: u32,
    timeout: Duration,
    observe: impl FnMut(Option<i32>),
) -> Result<()> {
    terminate_owned_foreground_group_with_io(group, timeout, signal_process_group, observe)
}

#[cfg(target_os = "macos")]
fn signal_process_group(target: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::kill(target, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, target_os = "macos"))]
fn terminate_owned_foreground_group_with_io(
    group: u32,
    timeout: Duration,
    signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
    observe: impl FnMut(Option<i32>),
) -> Result<()> {
    terminate_owned_foreground_group_until_with_io(group, Instant::now() + timeout, signal, observe)
}

#[cfg(target_os = "macos")]
fn terminate_owned_foreground_group_until_with_io(
    group: u32,
    deadline: Instant,
    mut signal: impl FnMut(libc::pid_t, libc::c_int) -> std::io::Result<()>,
    mut observe: impl FnMut(Option<i32>),
) -> Result<()> {
    close_remaining(deadline)?;
    let target = process_group_signal_target(group)?;
    if let Err(error) = signal(target, libc::SIGTERM) {
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error).context("could not send SIGTERM to the attested process group");
    }
    #[cfg(test)]
    session::close::interruption::at(session::close::interruption::Point::SignalSent);
    loop {
        if let Err(error) = signal(target, 0) {
            observe(error.raw_os_error());
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // EPERM does not establish absence, including for a zombie-only group
            // (measured on macOS 26.6.2, 2026-10-07, issue #85); only ESRCH does.
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(error)
                    .context("could not observe the attested process group after SIGTERM");
            }
        } else {
            observe(None);
        }
        if crate::native::session::close::budget::close_now() >= deadline {
            bail!("the attested process group was not confirmed gone before the deadline");
        }
        std::thread::sleep(Duration::from_millis(20).min(
            deadline.saturating_duration_since(crate::native::session::close::budget::close_now()),
        ));
    }
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
        owner.terminal_app =
            terminal::close_evidence(surface.kind).derive_app_incarnation(&owner)?;
        owner
    };
    Ok(owner)
}

#[cfg(all(test, target_os = "macos"))]
mod process_close_tests {
    use super::*;
    use std::{os::unix::process::CommandExt, process::Command};

    #[test]
    fn r3_replay_group_probe_does_not_accept_zombie_only_group() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        assert!(!owner_group_gone(group).unwrap());
        assert_eq!(unsafe { libc::kill(-(group as i32), libc::SIGTERM) }, 0);
        child.wait_for_zombie();
        assert!(!owner_group_gone(group).unwrap());
        drop(child);
        assert!(owner_group_gone(group).unwrap());
    }

    #[cfg(target_os = "macos")]
    struct ForegroundGroupChild(std::process::Child);

    #[cfg(target_os = "macos")]
    impl ForegroundGroupChild {
        fn spawn() -> Self {
            Self(
                Command::new("/bin/sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            )
        }

        fn wait_for_zombie(&self) {
            let target = -(self.0.id() as i32);
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if unsafe { libc::kill(target, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return;
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ForegroundGroupChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_waits_for_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let group = child.0.id();
        let (reap, ready) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let received = ready.recv_timeout(Duration::from_secs(5));
            drop(child);
            received
        });
        let mut probes = Vec::new();
        let mut eperm_probes = 0;
        let result =
            terminate_owned_foreground_group_with(group, Duration::from_secs(3), |errno| {
                probes.push(errno);
                if errno == Some(libc::EPERM) {
                    eperm_probes += 1;
                    if eperm_probes == 2 {
                        reap.send(()).unwrap();
                    }
                }
            });
        drop(reap);
        let reaped = waiter.join();
        assert!(result.is_ok(), "{result:?}");
        reaped.unwrap().unwrap();
        let gone = probes
            .iter()
            .position(|errno| *errno == Some(libc::ESRCH))
            .unwrap();
        assert!(
            probes[..gone]
                .iter()
                .filter(|errno| **errno == Some(libc::EPERM))
                .count()
                >= 2,
            "{probes:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_times_out_before_zombie_reaping() {
        let child = ForegroundGroupChild::spawn();
        let timeout = Duration::from_secs(1);
        let mut eperm_probes = 0;
        let started = Instant::now();
        let result = terminate_owned_foreground_group_with(child.0.id(), timeout, |errno| {
            if errno == Some(libc::EPERM) {
                eperm_probes += 1;
            }
        });
        let elapsed = started.elapsed();
        assert_eq!(
            result.as_ref().unwrap_err().to_string(),
            "the attested process group was not confirmed gone before the deadline",
            "{result:?}"
        );
        assert!(eperm_probes > 0, "no EPERM probe was observed");
        assert!(elapsed >= timeout);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_refuses_failed_signal_to_zombie() {
        let child = ForegroundGroupChild::spawn();
        assert_eq!(
            unsafe { libc::kill(-(child.0.id() as i32), libc::SIGTERM) },
            0
        );
        child.wait_for_zombie();
        let error = terminate_owned_foreground_group(child.0.id()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "could not send SIGTERM to the attested process group"
        );
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_foreground_group_termination_waits_for_real_private_process_exit() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id();
        let waiter = std::thread::spawn(move || child.wait().unwrap());
        let result = terminate_owned_foreground_group(group);
        let status = waiter.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "macos"))]
#[path = "ownership_teardown_tests.rs"]
mod teardown_tests;

#[cfg(all(test, target_os = "macos"))]
#[path = "ownership_close_interruption_tests.rs"]
mod close_interruption_tests;

#[cfg(all(test, target_os = "macos"))]
pub(in crate::native::terminal) fn test_owner_attestation(live: bool) -> OwnerAttestation {
    OwnerAttestation {
        process: if live {
            ProcessObservation::Live(crate::native::process_identity::Attested::Incarnation)
        } else {
            ProcessObservation::Gone(Gone::Exited)
        },
        control: crate::native::session::owner::ControlAttestation::NotRecorded,
        pid_alive: Some(live),
    }
}
