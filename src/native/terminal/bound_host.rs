#![cfg_attr(target_os = "linux", allow(dead_code))] // Phase 1 has no installed Linux surface.
use crate::native::private_to_user::owned_and_not_accessible_by_others;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(in crate::native::terminal) struct BoundHostSpec {
    pub(in crate::native::terminal) name: &'static str,
}

pub(in crate::native::terminal) fn run_tty_bound_host<I>(
    directory: &Path,
    spec: &BoundHostSpec,
    identity: impl FnOnce() -> Result<I>,
    bind: impl FnOnce(&Path, &str, &I) -> Result<()>,
    record_outcome: impl FnOnce(&Result<I>),
) -> Result<()> {
    run_bound_host(directory, spec, |id| {
        let outcome = (|| {
            let identity = identity()?;
            bind(directory, id, &identity)?;
            // Discard only this host's input after its exact surface has been bound.
            if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot discard {} startup input", spec.name));
            }
            Ok(identity)
        })();
        record_outcome(&outcome);
        outcome.map(|_| ())
    })
}

pub(super) fn run_bound_host<T>(
    directory: &Path,
    spec: &BoundHostSpec,
    run: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    run(admit(directory, spec)?)
}

/// One admission for every macOS/Linux host, in one order: an absolute path, a session-id file
/// name, an existing path, a real directory (never a symlink), private to the current user.
/// Decided 2026-10-30: the per-host orders and refusal texts this replaces changed which
/// refusal a defective directory reported; the refusal vocabulary is now one template.
fn admit<'a>(directory: &'a Path, spec: &BoundHostSpec) -> Result<&'a str> {
    let name = spec.name;
    if !directory.is_absolute() {
        bail!("{name} launch directory must be absolute");
    }
    let id = directory
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .with_context(|| format!("invalid {name} launch directory"))?;
    crate::native::session::primitives::require_valid_session_id(id)?;
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("no such {name} launch directory: {}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "refusing non-directory {name} launch path: {}",
            directory.display()
        );
    }
    if !owned_and_not_accessible_by_others(&metadata, unsafe { libc::geteuid() }) {
        bail!("{name} launch directory is not private to the current user");
    }
    Ok(id)
}

pub(super) fn binding_names_host(
    surface: &super::TerminalSession,
    kind: super::TerminalKind,
    names_host: impl FnOnce(&super::TerminalSession) -> bool,
    refusal: &'static str,
) -> Result<()> {
    if surface.kind != kind || !names_host(surface) {
        bail!("{refusal}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Phase 2 host handoff (no exec here): admit the private directory, wait for the
/// pending launch's binding, validate this host's adapter-owned environment against
/// it, then recheck the SAME pending Claim and exact binding under the lifecycle
/// lock before flushing startup input. The Linux command passes --launch-command
/// separately and execs bash --noprofile --norc -i -c only after this returns.
/// The wrapper's spawn fence still owns final launch admission; this is no reservation.
#[allow(dead_code)] // Phase 2 Linux host entry point.
pub(in crate::native::terminal) fn prepare_bound_launch(
    directory: &Path,
    spec: &BoundHostSpec,
    facts: &crate::native::session::launch::BindingFacts,
    clock: &mut dyn crate::native::clock::Clock,
    validate_binding: impl FnOnce(&super::TerminalSession) -> Result<()>,
) -> Result<super::TerminalSession> {
    prepare_bound_launch_with(directory, spec, facts, clock, validate_binding, || {
        if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot discard launch host startup input");
        }
        Ok(())
    })
}
fn prepare_bound_launch_with(
    directory: &Path,
    spec: &BoundHostSpec,
    facts: &crate::native::session::launch::BindingFacts,
    clock: &mut dyn crate::native::clock::Clock,
    validate_binding: impl FnOnce(&super::TerminalSession) -> Result<()>,
    flush: impl FnOnce() -> Result<()>,
) -> Result<super::TerminalSession> {
    use crate::native::session::{Store, launch};
    run_bound_host(directory, spec, |id| {
        let store = Store::open_unchecked(directory);
        let initial = launch::read(&store)?.context("missing host launch receipt")?;
        let surface = launch::wait_for_binding(&store, id, facts, clock)?;
        validate_binding(&surface)?;
        let _lock = store.lock()?;
        let current = launch::read(&store)?.context("missing host launch receipt")?;
        anyhow::ensure!(
            launch::pending::judge(
                &store,
                &current,
                &initial.claim_token,
                launch::pending::Expiry::Receipt
            )?
            .is_ok(),
            "launch host is no longer pending"
        );
        anyhow::ensure!(
            launch::pending::bound_surface(&store, id, || "invalid host binding")?.as_ref()
                == Some(&surface),
            "launch host binding changed"
        );
        flush()?;
        Ok(surface)
    })
}
