#[cfg(unix)]
use crate::native::private_to_user::owned_and_not_accessible_by_others;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(in crate::native::terminal) struct BoundHostSpec {
    pub(in crate::native::terminal) name: &'static str,
}

#[cfg(target_os = "macos")]
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

/// One admission for every bound host, in one order: an absolute path, a session-id file
/// name, an existing path, a real directory (never a symlink), private to the current user.
/// On Windows the last two are read again from a handle opened on the directory itself, not
/// through a reparse point: no reparse point, and the trust store's access-list rule with
/// the rights that change a directory's entries (`consent::windows_directory_is_private`).
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
    #[cfg(unix)]
    if !owned_and_not_accessible_by_others(&metadata, unsafe { libc::geteuid() }) {
        bail!("{name} launch directory is not private to the current user");
    }
    #[cfg(windows)]
    windows_private_directory(directory, name)?;
    Ok(id)
}

// The directory as the handle that opened it reports it, so that a reparse point or a
// directory replaced after the path was read is seen. A failure to open it or to read its
// access list refuses the launch.
#[cfg(windows)]
fn windows_private_directory(directory: &Path, name: &str) -> Result<()> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let opened = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(directory)
        .with_context(|| {
            format!(
                "cannot open {name} launch directory {}",
                directory.display()
            )
        })?;
    let metadata = opened.metadata().with_context(|| {
        format!(
            "cannot inspect {name} launch directory {}",
            directory.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!(
            "refusing non-directory {name} launch path: {}",
            directory.display()
        );
    }
    crate::native::consent::windows_directory_is_private(&opened)
        .with_context(|| format!("{name} launch directory is not private to the current user"))
}

#[cfg(target_os = "macos")]
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

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;

/// The tmux host's handoff (no exec here): admit the private directory, wait for the
/// pending launch's binding, validate this host's adapter-owned environment against
/// it, then recheck the SAME pending Claim and exact binding under the lifecycle
/// lock before flushing startup input. The host is given its launch command through
/// --launch-command and execs bash only after this returns.
/// The wrapper's spawn fence still owns final launch admission; this is no reservation.
#[cfg(target_os = "linux")]
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
                .with_context(|| format!("cannot discard {} startup input", spec.name));
        }
        Ok(())
    })
}
/// The Windows Herdr host's handoff, in the same order: after its exact binding, the
/// console input typed into the new terminal is discarded, so that an Enter cannot answer
/// the first dialog the provider shows. A console that cannot be flushed refuses the launch.
#[cfg(windows)]
pub(in crate::native::terminal) fn prepare_bound_launch(
    directory: &Path,
    spec: &BoundHostSpec,
    facts: &crate::native::session::launch::BindingFacts,
    clock: &mut dyn crate::native::clock::Clock,
    validate_binding: impl FnOnce(&super::TerminalSession) -> Result<()>,
) -> Result<super::TerminalSession> {
    use std::os::windows::io::AsRawHandle;
    prepare_bound_launch_with(directory, spec, facts, clock, validate_binding, || {
        let input = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .with_context(|| format!("cannot open the {} console input", spec.name))?;
        if unsafe {
            windows_sys::Win32::System::Console::FlushConsoleInputBuffer(input.as_raw_handle())
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot discard {} startup input", spec.name));
        }
        Ok(())
    })
}

// Compiled into every test build, so that macOS runs the handoff tests too.
#[cfg(any(target_os = "linux", windows, test))]
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
        let name = spec.name;
        let initial =
            launch::read(&store)?.with_context(|| format!("missing {name} launch receipt"))?;
        let surface = launch::wait_for_binding(&store, id, facts, clock)?;
        validate_binding(&surface)?;
        let _lock = store.lock()?;
        let current =
            launch::read(&store)?.with_context(|| format!("missing {name} launch receipt"))?;
        anyhow::ensure!(
            launch::pending::judge(
                &store,
                &current,
                &initial.claim_token,
                launch::pending::Expiry::Receipt
            )?
            .is_ok(),
            "{name} launch is no longer pending"
        );
        anyhow::ensure!(
            launch::pending::bound_surface(&store, id, || "invalid host binding")?.as_ref()
                == Some(&surface),
            "{name} binding changed"
        );
        flush()?;
        Ok(surface)
    })
}
