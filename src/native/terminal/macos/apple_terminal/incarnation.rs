//! Terminal.app window and TTY identities are scoped to one app incarnation. This module
//! captures it from the shell's ancestry, decides whether it has ended, and owns the order
//! of that decision around a close transaction and around an absence observation; the two
//! orders differ and stay separate.
use crate::native::session::Store;
use crate::native::session::close::budget::close_remaining;
use crate::native::terminal::macos::process::{
    macos_process_path, macos_process_records, macos_processes_named,
};
use crate::native::terminal::ownership::{
    MacTerminalAppIdentity, MacTerminalShellIdentity, NativeSessionOwner,
};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Instant;

// Recheck before every transaction, including retries; a restarted app must
// never inherit the old window/tty's close authority. `None` is an ended incarnation:
// the effect was not run, or its reply is discarded.
pub(super) fn around_close(
    app: &MacTerminalAppIdentity,
    deadline: Instant,
    mut process_birth: impl FnMut(u32) -> Result<Option<(u64, u64)>>,
    mut instances: impl FnMut() -> Result<Vec<MacTerminalAppIdentity>>,
    effect: impl FnOnce() -> Result<String>,
) -> Result<Option<String>> {
    close_remaining(deadline)?;
    if terminal_app_ended(app, &mut process_birth, &mut instances)? {
        return Ok(None);
    }
    close_remaining(deadline)?;
    let reply = effect()?;
    close_remaining(deadline)?;
    if terminal_app_ended(app, &mut process_birth, &mut instances)? {
        return Ok(None);
    }
    close_remaining(deadline)?;
    Ok(Some(reply))
}

// An ended incarnation is proven by its own birth record; a live one must be the only
// running Terminal, which `application "Terminal"` then addresses.
pub(super) fn terminal_app_ended(
    app: &MacTerminalAppIdentity,
    process_birth: impl FnOnce(u32) -> Result<Option<(u64, u64)>>,
    instances: impl FnOnce() -> Result<Vec<MacTerminalAppIdentity>>,
) -> Result<bool> {
    if !terminal_app_alive_with(app, process_birth)? {
        return Ok(true);
    }
    require_unique_terminal_app(app, &instances()?)?;
    Ok(false)
}

// The close of a live owner: a legacy owner's incarnation is recorded first, and the
// recorded app must then be the only running instance.
pub(super) fn record_terminal_app_for_close(
    directory: &Path,
    owner: &mut NativeSessionOwner,
) -> Result<()> {
    record_terminal_app_for_close_with(
        directory,
        owner,
        terminal_app_process,
        terminal_app_instances,
    )
}

fn record_terminal_app_for_close_with(
    directory: &Path,
    owner: &mut NativeSessionOwner,
    derive: impl FnOnce(&NativeSessionOwner) -> Result<MacTerminalAppIdentity>,
    instances: impl FnOnce() -> Result<Vec<MacTerminalAppIdentity>>,
) -> Result<()> {
    record_legacy_terminal_app(directory, owner, derive)?;
    require_unique_terminal_app(
        owner
            .terminal_app
            .as_ref()
            .context("Terminal.app identity was not recorded")?,
        &instances()?,
    )
}

// Without a recorded app incarnation, attest the live owner and persist its incarnation
// before close intent so every retry carries it.
fn record_legacy_terminal_app(
    directory: &Path,
    owner: &mut NativeSessionOwner,
    derive: impl FnOnce(&NativeSessionOwner) -> Result<MacTerminalAppIdentity>,
) -> Result<()> {
    if owner.terminal_app.is_none() {
        owner.terminal_app = Some(derive(owner)?);
        Store::open_unchecked(directory).write_owner(owner)?;
    }
    Ok(())
}

// One ancestor of the terminal shell: its incarnation, its parent and its executable.
pub(in crate::native::terminal::macos) fn terminal_app_ancestor(
    pid: u32,
) -> Result<(MacTerminalAppIdentity, u32, String)> {
    let mut name = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        libc::c_int::try_from(pid).context("PID is out of range")?,
    ];
    let records = macos_process_records(&mut name)
        .with_context(|| format!("failed to inspect process {pid}"))?;
    let [record] = records.as_slice() else {
        bail!("Terminal.app ancestor {pid} ended during attestation")
    };
    let identity = record.identity()?;
    if identity.pid != pid {
        bail!("macOS returned the identity of another process for {pid}")
    }
    let path = macos_process_path(pid)?
        .with_context(|| format!("Terminal.app ancestor {pid} ended during attestation"))?;
    Ok((identity, u32::try_from(record.parent_pid)?, path))
}

// Every running process that `application "Terminal"` can be.
pub(super) fn terminal_app_instances() -> Result<Vec<MacTerminalAppIdentity>> {
    macos_processes_named(b"Terminal")
}

fn require_unique_terminal_app(
    app: &MacTerminalAppIdentity,
    instances: &[MacTerminalAppIdentity],
) -> Result<()> {
    if instances != std::slice::from_ref(app) {
        bail!(
            "Terminal.app scripting target is ambiguous or changed; the recorded app must be the only running instance and the exact surface handle is retained"
        );
    }
    Ok(())
}

pub(super) fn terminal_app_process(owner: &NativeSessionOwner) -> Result<MacTerminalAppIdentity> {
    let shell = owner
        .terminal_shell
        .as_ref()
        .context("Terminal.app owner has no shell identity")?;
    terminal_app_process_with(shell, terminal_app_ancestor)
}

fn terminal_app_process_with(
    shell: &MacTerminalShellIdentity,
    mut observe: impl FnMut(u32) -> Result<(MacTerminalAppIdentity, u32, String)>,
) -> Result<MacTerminalAppIdentity> {
    let mut pid = shell.pid;
    let mut lineage = Vec::new();
    while pid > 1
        && !lineage
            .iter()
            .any(|(app, _, _): &(MacTerminalAppIdentity, u32, String)| app.pid == pid)
    {
        let (app, parent, path) = observe(pid)?;
        if app.pid != pid
            || (pid == shell.pid
                && (app.start_seconds, app.start_microseconds)
                    != (
                        shell.process_start_seconds,
                        shell.process_start_microseconds,
                    ))
        {
            bail!("Terminal.app ancestor incarnation changed during app attestation");
        }
        let is_terminal = matches!(
            path.as_str(),
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"
                | "/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"
        );
        lineage.push((app.clone(), parent, path));
        if is_terminal {
            for recorded in &lineage {
                if observe(recorded.0.pid)? != *recorded {
                    bail!("Terminal.app ancestor identity changed during attestation");
                }
            }
            return Ok(app);
        }
        pid = parent;
    }
    bail!("the verified terminal shell has no system Terminal.app ancestor")
}

fn terminal_app_alive_with(
    app: &MacTerminalAppIdentity,
    process_birth: impl FnOnce(u32) -> Result<Option<(u64, u64)>>,
) -> Result<bool> {
    if app.pid == 0 || app.start_seconds == 0 || app.start_microseconds >= 1_000_000 {
        bail!(
            "Terminal.app process incarnation is incomplete; the exact surface handle is retained"
        );
    }
    use crate::native::process_identity::{self, Gone, ProcessObservation};
    match process_identity::classify(
        app.pid,
        Some(&(app.start_seconds, app.start_microseconds)),
        || false, // A recorded incarnation never uses PID-only liveness.
        || process_birth(app.pid),
    ) {
        ProcessObservation::Live(_) => Ok(true),
        ProcessObservation::Gone(Gone::Exited) => Ok(false),
        ProcessObservation::Gone(Gone::Reused(_)) => {
            bail!("Terminal.app PID was reused; no close or absence authority was granted")
        }
        ProcessObservation::Unknown(reason) => bail!("{reason}"),
    }
}

// Without an incarnation, require at most one unchanged Terminal process and an absent id.
// A listed id can be reused; see docs/verification/2026-10-03-v0.1.0-focus.md.
// With one, the incarnation is rechecked before and after the presence query, with no
// deadline of its own.
pub(super) fn terminal_surface_absent(
    app: Option<&MacTerminalAppIdentity>,
    process_birth: impl Fn(u32) -> Result<Option<(u64, u64)>>,
    terminal_instances: impl Fn() -> Result<Vec<MacTerminalAppIdentity>>,
    surface_present: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    let Some(app) = app else {
        let before = terminal_instances()?;
        if before.len() > 1 {
            bail!(
                "more than one Terminal.app process is running and the record names none; absence is unproven and the surface handle is retained"
            );
        }
        let present = surface_present()?;
        if terminal_instances()? != before {
            bail!(
                "Terminal.app started or ended during the observation; absence is unproven and the surface handle is retained"
            );
        }
        return Ok(!present);
    };
    if terminal_app_ended(app, &process_birth, &terminal_instances)? {
        return Ok(true);
    }
    let present = surface_present()?;
    if terminal_app_ended(app, &process_birth, &terminal_instances)? {
        return Ok(true);
    }
    Ok(!present)
}

#[cfg(test)]
mod tests;
