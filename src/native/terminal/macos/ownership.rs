use super::process::{live_native_process_identity, terminal_tty_device};
use crate::native::session::{self, CoreRecord, Reader};
use crate::native::terminal;
use crate::native::terminal::ownership::{
    NativeProcessIdentity, NativeSessionOwner, native_owner_identity_matches,
    verified_terminal_owner_process_group, verified_terminal_shell_process_group,
};
use agent_bridge::process_is_alive;
use anyhow::{Context, Result, bail};
use std::path::Path;

#[cfg(target_os = "macos")]
pub(in crate::native) fn verified_macos_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let owner_path = Reader::open_unchecked(directory)
        .record(CoreRecord::Owner)
        .path()
        .to_owned();
    let owner_text = session::RecordReader::at(&owner_path)
        .text()?
        .with_context(|| "Terminal.app ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    let live = live_native_process_identity(owner.pid)?;
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
    if terminal_tty_device(Path::new(owner_tty))? != live.terminal_tty_device {
        bail!("native-session owner TTY path no longer identifies its controlling TTY")
    }
    verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = live_native_process_identity(live.parent_pid)?;
    verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    Ok((owner, live))
}

#[cfg(target_os = "macos")]
pub(in crate::native) fn mac_native_owner_is_live(owner: &NativeSessionOwner) -> Result<bool> {
    if matches!(
        (
            owner.terminal_tty_device,
            owner.process_start_seconds,
            owner.process_start_microseconds,
            owner.process_group,
            owner.terminal_process_group,
        ),
        (None, None, None, None, None)
    ) {
        // Pre-0.0.3 owner records had only a PID. Preserve their historical liveness
        // behavior; every newly launched macOS session records the strong identity below.
        return Ok(process_is_alive(owner.pid));
    }
    match live_native_process_identity(owner.pid) {
        Ok(live) => Ok(native_owner_identity_matches(owner, &live)),
        Err(error) if process_is_alive(owner.pid) => Err(error).with_context(|| {
            format!(
                "failed to verify the birth identity of live native-session process {}",
                owner.pid
            )
        }),
        Err(_) => Ok(false),
    }
}
