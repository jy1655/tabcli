//! Bound hosts share private-directory admission, binding observation and refusal reporting.
//! The direct handoffs also recheck the pending Claim and exact binding under the lifecycle
//! lock before flushing input. The wrapper owns final spawn admission in every mode.
#[cfg(any(target_os = "linux", windows, test))]
use super::record::HerdrHostBirth;
use super::{TerminalKind, TerminalSession};
use crate::native::clock::Clock;
#[cfg(unix)]
use crate::native::private_to_user::owned_and_not_accessible_by_others;
#[cfg(any(target_os = "linux", windows, test))]
use crate::native::session::launch::host::FailureReason;
use crate::native::session::{Reader, Store, launch};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(in crate::native::terminal) struct BoundHostSpec {
    pub(in crate::native::terminal) name: &'static str,
}

/// Dispatch the command form through the kind's registered binding proof and local handoff.
pub(super) fn run_launch_host(directory: &Path, command: &str, kind: TerminalKind) -> Result<()> {
    #[cfg(target_os = "macos")]
    if kind.captures_shell_environment() {
        return super::macos::launch_host::run(directory, command, kind);
    }
    #[cfg(any(target_os = "linux", windows))]
    if let Some(names_binding) = kind
        .registration()
        .launch_host
        .and_then(|host| host.names_binding)
    {
        #[cfg(target_os = "linux")]
        let handoff = super::linux::host::handoff;
        #[cfg(windows)]
        let handoff = super::windows::launch_host::start_root;
        let result = run_handoff(
            directory,
            kind,
            |surface| {
                anyhow::ensure!(
                    surface.kind == kind && names_binding(surface),
                    "{} surface binding does not name this launch host",
                    kind.display_name()
                );
                Ok(())
            },
            || handoff(command, kind),
        );
        #[cfg(target_os = "linux")]
        return result;
        #[cfg(windows)]
        return super::windows::launch_host::wait_for_root(result?).inspect_err(|error| {
            log_refusal(
                &Store::open_unchecked(directory),
                kind,
                &format_args!(": {error:#}"),
            );
        });
    }
    bail!("{}", super::host::UNSUPPORTED_LAUNCH_HOST)
}

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) fn run_tty_bound_host<I>(
    directory: &Path,
    kind: TerminalKind,
    identity: impl FnOnce() -> Result<I>,
    bind: impl FnOnce(&Path, &str, &I) -> Result<()>,
    release_message: Option<fn(&I) -> String>,
) -> Result<()> {
    run_logged_bound_host(directory, kind, |id| {
        let outcome = (|| {
            let identity = identity()?;
            bind(directory, id, &identity)?;
            flush_startup_input(kind.display_name())?;
            Ok(identity)
        })();
        let store = Store::open_unchecked(directory);
        match outcome {
            Ok(identity) => {
                if let Some(message) = release_message {
                    launch::log(&store, &message(&identity));
                }
                Ok(())
            }
            Err(error) => {
                log_refusal(&store, kind, &format_args!(": {error:#}"));
                Err(error)
            }
        }
    })
}

/// Keep the legacy log prefix for Terminal.app.
pub(super) fn log_refusal(store: &Store, kind: TerminalKind, detail: &impl std::fmt::Display) {
    let name = if kind == TerminalKind::AppleTerminal {
        "terminal"
    } else {
        kind.as_str()
    };
    launch::log(store, &format!("{name}_host_refused{detail}"));
}

/// The receipt observes the binding; the adapter proves that it names this host.
pub(super) fn wait_for_host_binding(
    reader: &Reader,
    id: &str,
    kind: TerminalKind,
    clock: &mut dyn Clock,
    validate: impl FnOnce(&TerminalSession) -> Result<()>,
) -> Result<TerminalSession> {
    let surface = launch::wait_for_binding(reader, id, kind.registration().binding(), clock)?;
    validate(&surface)?;
    Ok(surface)
}

#[cfg(target_os = "macos")]
pub(super) fn while_capture_eligible<T>(
    store: &Store,
    token: &str,
    identity: launch::host::HostIdentity,
    act: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = store.lock()?;
    launch::host::eligible_incarnation(store, token, identity)?;
    act()
}

// The Ghostty and Warp hosts, which report their own refusals.
#[cfg(any(target_os = "macos", test))]
pub(super) fn run_bound_host<T>(
    directory: &Path,
    spec: &BoundHostSpec,
    run: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    run(admit(directory, spec)?)
}

/// `run_bound_host` for a launch host whose admission refusal goes to the launch log, on
/// every platform: the Linux and Windows handoffs, the macOS capture host and the tty-bound
/// hosts. The line is a diagnostic, never a launch Record. It is written only into a real
/// directory at an absolute path: a link or anything else at that path can name another
/// session, and a refusal never writes through what it refused.
pub(super) fn run_logged_bound_host<T>(
    directory: &Path,
    kind: TerminalKind,
    run: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    let spec = BoundHostSpec {
        name: kind.display_name(),
    };
    let id = admit(directory, &spec).inspect_err(|error| {
        let real_directory = directory.is_absolute()
            && std::fs::symlink_metadata(directory)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        if real_directory {
            log_refusal(
                &Store::open_unchecked(directory),
                kind,
                &format_args!(": {error:#}"),
            );
        }
    })?;
    run(id)
}

/// The account a launch host runs as, for the private-directory step of its admission: an
/// effective uid on macOS and Linux; on Windows the owners its token gives new files and,
/// only to explain a refusal, whether it is elevated.
#[derive(Clone, Debug)]
pub(in crate::native::terminal) struct HostAccount {
    #[cfg(unix)]
    uid: u32,
    #[cfg(windows)]
    windows: crate::native::consent::WindowsAccount,
}

impl HostAccount {
    /// This process's account.
    pub(in crate::native::terminal) fn current() -> Result<Self> {
        #[cfg(unix)]
        return Ok(Self {
            uid: unsafe { libc::geteuid() },
        });
        #[cfg(windows)]
        return Ok(Self {
            windows: crate::native::consent::windows_current_account()?,
        });
    }

    #[cfg(unix)]
    pub(in crate::native::terminal) fn of_uid(uid: u32) -> Self {
        Self { uid }
    }

    #[cfg(windows)]
    pub(in crate::native::terminal) fn of_windows(
        windows: crate::native::consent::WindowsAccount,
    ) -> Self {
        Self { windows }
    }

    /// Whether the two accounts give their files the same owners.
    pub(in crate::native::terminal) fn same(&self, other: &Self) -> bool {
        #[cfg(unix)]
        return self.uid == other.uid;
        #[cfg(windows)]
        return self.windows.same_owners(&other.windows);
    }
}

/// Whose account a session directory is judged for: the host's own (this process, as the
/// host itself reads it), a given one, or none known. Without a known account only what does
/// not depend on one is judged (the path, a real directory, and on macOS and Linux the
/// permission bits that let any other account in); the owner is left to the host.
#[derive(Clone, Copy)]
enum Judged<'a> {
    Own,
    For(&'a HostAccount),
    AccountUnknown,
}

/// How the launcher judges the session directory of a kind's launch host before it opens a
/// surface, on this platform. A host this platform does not run judges nothing here, so an
/// unsupported surface keeps its own refusal. tmux runs its host in a server this process
/// starts, so as this process's account; TAB does not attest the account of a macOS terminal
/// application, so only what needs no account is judged; Herdr's host runs as its core,
/// which the adapter judges on the connection that creates the terminal
/// (`refuse_before_core_launch`); the Windows console host is no bound host.
fn judged_by_the_launcher(kind: TerminalKind) -> Option<Judged<'static>> {
    match kind {
        TerminalKind::Tmux => cfg!(target_os = "linux").then_some(Judged::Own),
        TerminalKind::Iterm2
        | TerminalKind::WezTerm
        | TerminalKind::AppleTerminal
        | TerminalKind::Ghostty
        | TerminalKind::Warp => cfg!(target_os = "macos").then_some(Judged::AccountUnknown),
        TerminalKind::Herdr | TerminalKind::WindowsConsole => None,
    }
}

/// The launcher's check before it creates a surface whose launch host runs `admit`: the
/// same admission, as far as the launcher can establish the host's account
/// (`judged_by_the_launcher`). A refusal here is one the host would make, so nothing is
/// created; it is a `RefusedDirectory`.
pub(in crate::native::terminal) fn refuse_before_launch(
    directory: &Path,
    kind: TerminalKind,
) -> Result<()> {
    let Some(judged) = judged_by_the_launcher(kind) else {
        return Ok(());
    };
    let spec = BoundHostSpec {
        name: kind.display_name(),
    };
    match admit_as(directory, &spec, judged) {
        Ok(_) => Ok(()),
        Err(refusal) => Err(would_refuse(directory, &spec, &refusal, None)),
    }
}

/// The same check for a host that a running core starts as its own program, judged for
/// `core`, the account of the process serving the connection that will create the terminal,
/// or, when that connection could not name it, only for what needs no account. When this
/// process's own account would be admitted, the refusal also says how the two accounts
/// differ: on Windows an elevated TAB normally gives its files the Administrators group as
/// owner, which the launch host of a core that is not elevated refuses (observed 2026-11-26,
/// hc 0.1.21).
pub(in crate::native::terminal) fn refuse_before_core_launch(
    directory: &Path,
    kind: TerminalKind,
    core: Option<&HostAccount>,
) -> Result<()> {
    let spec = BoundHostSpec {
        name: kind.display_name(),
    };
    let judged = core.map_or(Judged::AccountUnknown, Judged::For);
    let Err(refusal) = admit_as(directory, &spec, judged) else {
        return Ok(());
    };
    let advice = core.and_then(|core| {
        HostAccount::current()
            .ok()
            .filter(|own| !own.same(core) && admit_as(directory, &spec, Judged::For(own)).is_ok())
            .map(|own| account_advice(spec.name, &own, core))
    });
    Err(would_refuse(directory, &spec, &refusal, advice))
}

fn would_refuse(
    directory: &Path,
    spec: &BoundHostSpec,
    refusal: &anyhow::Error,
    advice: Option<String>,
) -> anyhow::Error {
    let mut message = format!(
        "the {} launch host would refuse the session directory {}: {refusal:#}",
        spec.name,
        directory.display()
    );
    if let Some(advice) = advice {
        message.push_str("; ");
        message.push_str(&advice);
    }
    super::RefusedDirectory::new(message).into()
}

#[cfg(unix)]
fn account_advice(name: &str, own: &HostAccount, core: &HostAccount) -> String {
    format!(
        "TAB runs as uid {} and the {name} Core as uid {}; run TAB as the core's user",
        own.uid, core.uid
    )
}

#[cfg(windows)]
fn account_advice(name: &str, own: &HostAccount, core: &HostAccount) -> String {
    elevation_advice(name, own.windows.elevated, core.windows.elevated)
}

/// What a user can do when TAB and the core give their files different owners on Windows.
#[cfg(any(windows, test))]
fn elevation_advice(name: &str, tab: Option<bool>, core: Option<bool>) -> String {
    if (tab, core) == (Some(true), Some(false)) {
        return format!(
            "TAB is running elevated and the {name} Core is not: an elevated process gives its files the default owner of its token (normally the Administrators group), which the core's launch host does not accept as its user's. Run TAB without elevation, as the core runs (an administrator's SSH session is elevated)"
        );
    }
    let word = |elevated: Option<bool>| match elevated {
        Some(true) => "elevated",
        Some(false) => "not elevated",
        None => "elevation unknown",
    };
    format!(
        "TAB ({}) and the {name} Core ({}) give their files different owners; run TAB with the core's account and elevation",
        word(tab),
        word(core)
    )
}

/// One admission for every bound host, in one order: an absolute path, a session-id file
/// name, an existing path, a real directory (never a symlink), private to the current user.
/// On Windows the last two are read again from a handle opened on the directory itself, not
/// through a reparse point: no reparse point, and the trust store's access-list rule with
/// the rights that change a directory's entries (`consent::windows_directory_is_private`).
/// Decided 2026-10-30: the per-host orders and refusal texts this replaces changed which
/// refusal a defective directory reported; the refusal vocabulary is now one template.
fn admit<'a>(directory: &'a Path, spec: &BoundHostSpec) -> Result<&'a str> {
    admit_as(directory, spec, Judged::Own)
}

/// `admit` for the account the host runs as (`Judged`).
fn admit_as<'a>(directory: &'a Path, spec: &BoundHostSpec, judged: Judged<'_>) -> Result<&'a str> {
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
    {
        use std::os::unix::fs::MetadataExt;
        let private = match judged {
            Judged::Own => {
                owned_and_not_accessible_by_others(&metadata, unsafe { libc::geteuid() })
            }
            Judged::For(account) => owned_and_not_accessible_by_others(&metadata, account.uid),
            Judged::AccountUnknown => metadata.mode() & 0o077 == 0,
        };
        if !private {
            bail!("{name} launch directory is not private to the current user");
        }
    }
    #[cfg(windows)]
    windows_private_directory(directory, name, judged)?;
    Ok(id)
}

// The directory as the handle that opened it reports it, so that a reparse point or a
// directory replaced after the path was read is seen. A failure to open it or to read its
// access list refuses the launch.
#[cfg(windows)]
fn windows_private_directory(directory: &Path, name: &str, judged: Judged<'_>) -> Result<()> {
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
    match judged {
        Judged::Own => crate::native::consent::windows_directory_is_private(&opened),
        Judged::For(account) => {
            crate::native::consent::windows_directory_is_private_to(&opened, || {
                Ok(account.windows.owners.clone())
            })
        }
        // Who may change a directory is judged against the owners of the account that trusts
        // it; without them the host decides.
        Judged::AccountUnknown => Ok(()),
    }
    .with_context(|| format!("{name} launch directory is not private to the current user"))
}

#[cfg(test)]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;

#[cfg(unix)]
pub(super) fn flush_startup_input(name: &str) -> Result<()> {
    if unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot discard {name} startup input"));
    }
    Ok(())
}

#[cfg(windows)]
fn flush_startup_input(name: &str) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .with_context(|| format!("cannot open the {name} console input"))?;
    if unsafe {
        windows_sys::Win32::System::Console::FlushConsoleInputBuffer(input.as_raw_handle())
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot discard {name} startup input"));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", windows))]
pub(super) fn run_handoff<T>(
    directory: &Path,
    kind: TerminalKind,
    validate: impl FnOnce(&TerminalSession) -> Result<()>,
    handoff: impl FnOnce() -> Result<T>,
) -> Result<T> {
    run_handoff_with(
        directory,
        kind,
        &mut crate::native::clock::SystemClock,
        validate,
        || flush_startup_input(kind.display_name()),
        handoff,
        || {
            use super::herdr::{Core, Installed};
            Installed.process_start(std::process::id()).ok().flatten()
        },
    )
}

/// Admit, wait, validate and recheck before touching the startup input queue. The handoff
/// starts the shell outside that lock. Windows waits for the root only after it has started.
/// Only an attested Herdr host's flush or handoff failure is durable. An unbound or
/// mismatched process has no authority to make the launch fail.
#[cfg(any(target_os = "linux", windows, test))]
fn run_handoff_with<T>(
    directory: &Path,
    kind: TerminalKind,
    clock: &mut dyn Clock,
    validate: impl FnOnce(&TerminalSession) -> Result<()>,
    flush: impl FnOnce() -> Result<()>,
    handoff: impl FnOnce() -> Result<T>,
    current_host_birth: impl FnOnce() -> Option<HerdrHostBirth>,
) -> Result<T> {
    use launch::pending::{self, Expiry};

    let name = kind.display_name();
    let store = Store::open_unchecked(directory);
    let start = clock.now();
    // Direct hosts have always logged admission refusals as well as later errors; the
    // admission's own refusal only into a real directory (`run_logged_bound_host`).
    let refused = |error: &anyhow::Error| log_refusal(&store, kind, &format_args!(": {error:#}"));
    let (initial, surface, flushed) = run_logged_bound_host(directory, kind, |id| {
        (|| {
            let initial =
                launch::read(&store)?.with_context(|| format!("missing {name} launch receipt"))?;
            let surface = wait_for_host_binding(&store, id, kind, clock, validate)?;
            let _lock = store.lock()?;
            let current =
                launch::read(&store)?.with_context(|| format!("missing {name} launch receipt"))?;
            anyhow::ensure!(
                pending::judge(&store, &current, &initial.claim_token, Expiry::Receipt)?.is_ok(),
                "{name} launch is no longer pending"
            );
            anyhow::ensure!(
                pending::bound_surface(&store, id, || "invalid host binding")?.as_ref()
                    == Some(&surface),
                "{name} binding changed"
            );
            Ok((initial, surface, flush()))
        })()
        .inspect_err(refused)
    })?;

    let (reason, outcome) = match flushed {
        Ok(()) => (FailureReason::LaunchShellExecFailed, handoff()),
        Err(error) => (FailureReason::InputFlushFailed, Err(error)),
    };
    outcome.inspect_err(|error| {
        // The launcher can return as soon as the Record appears. Attempt the log first
        // so the details are available to that caller.
        refused(error);
        if kind == TerminalKind::Herdr
            && surface.herdr_core().is_some_and(|core| {
                core.host_pid == std::process::id() && current_host_birth() == Some(core.birth)
            })
        {
            let _ = launch::host::fail_bound_herdr(
                &store,
                &initial.claim_token,
                &surface,
                reason,
                clock.now().saturating_duration_since(start).as_millis() as u64,
            );
        }
    })
}
