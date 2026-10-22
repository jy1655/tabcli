//! The wrapper operation of the launch protocol: the native session command as the bootstrap
//! runs it in the surface. Admission, the fd handoff, the owner record, the version gate,
//! the provider plan, the spawn with its durable identity and the exit settlement.
use super::WRAPPER_EXIT_CODE_MARKER;
use crate::native::reopen::{ReopenGate, read_resumed_from, record_reopen_refusal};
use crate::native::session::primitives::{SESSION_DIR_ENV, unix_ms};
use crate::native::session::schema::ProviderProcessRecord;
use crate::native::session::state::{SessionState, update_status};
use crate::native::session::{self, CoreRecord, Reader, Store};
use crate::native::{process_env, provider, terminal};
use agent_bridge::{FirstPartyCli, cli_version_is_supported};
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::fs::File;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::Instant,
};

pub(in crate::native) const STDERR_ENV: &str = "AGENT_BRIDGE_LAUNCH_STDERR_FD";
pub(in crate::native) const STDOUT_ENV: &str = "AGENT_BRIDGE_LAUNCH_STDOUT_FD";

// Records the spawned provider process before the session leaves its launch state. The
// caller still holds the child handle, so on Windows the pid cannot have been reused
// between the spawn and the identity query. The record names the process Bridge spawned:
// for a native executable that is the provider itself; for a `.ps1` or `.cmd` shim it is
// the shim, which waits on the provider and whose own survival is what the record can
// verify.
pub(in crate::native) fn record_provider_process(
    directory: &Path,
    managed_session_id: &str,
    child: &std::process::Child,
) -> Result<()> {
    let pid = child.id();
    #[cfg(windows)]
    let windows_process_identity = Some(
        terminal::windows_process_identity(pid)
            .with_context(|| format!("failed to record the identity of provider process {pid}"))?,
    );
    #[cfg(not(windows))]
    let windows_process_identity = None;
    Store::open_unchecked(directory)
        .write_provider_process(&ProviderProcessRecord {
            schema: 1,
            managed_session_id: managed_session_id.to_owned(),
            pid,
            windows_process_identity,
            spawned_unix_ms: unix_ms(),
        })
        .context("failed to record the spawned provider process")
}

pub(in crate::native) fn run(id: &str) -> Result<()> {
    let directory = Reader::session_directory(id)?;
    super::log(&Store::open_unchecked(&directory), "wrapper_started");
    super::admit_wrapper(&Store::open_unchecked(&directory))?;
    let result = (|| {
        restore_stdout()?;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("native-session must run in a visible interactive terminal");
        }
        let owner = terminal::ownership::current_surface_owner(&directory, id)?;
        Store::open_unchecked(&directory).write_owner(&owner)?;
        super::log(&Store::open_unchecked(&directory), "owner_recorded");
        provision(&directory)
    })();
    match &result {
        Ok(()) => super::log(
            &Store::open_unchecked(&directory),
            &format!("{WRAPPER_EXIT_CODE_MARKER}0"),
        ),
        Err(error) => super::log(
            &Store::open_unchecked(&directory),
            &format!("{WRAPPER_EXIT_CODE_MARKER}1; {error:#}"),
        ),
    }
    if let Err(finalization_error) = super::finalize(&Store::open_unchecked(&directory), &result) {
        return match result {
            Ok(()) => Err(finalization_error),
            Err(run_error) => Err(anyhow::anyhow!(
                "{run_error:#}; native session finalization also failed: {finalization_error:#}"
            )),
        };
    }
    result
}

pub(in crate::native) fn provision(directory: &Path) -> Result<()> {
    let manifest = Reader::open_unchecked(directory).manifest()?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = Reader::open_unchecked(directory)
        .record(CoreRecord::InitialPrompt)
        .path()
        .to_owned();
    let prompt = Reader::open_unchecked(directory)
        .initial_prompt()
        .context("failed to read initial prompt")?;
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    // The adapter produces the complete provider command: the policy flags of the manifest,
    // its own arguments and the prompt's place. A reopened session launches through the
    // adapter's resume plan with the new request's policy; the plan carries no prompt, and
    // the reopened process receives it through the same transport a fresh launch uses.
    let resumed_from = read_resumed_from(directory)?;
    let (arguments, initial_prompt, watcher, environment_removals) = match &resumed_from {
        Some(resumed_from) => {
            let provider::ResumePlan {
                arguments,
                initial_prompt,
                watcher,
                environment_removals,
            } = provider::reopen(provider)?.prepare(provider::ResumeContext {
                bridge_executable: &executable,
                directory,
                provider_session_id: &resumed_from.provider_session_id,
                yolo: manifest.yolo,
                model: manifest.model.as_deref(),
                effort: manifest.effort.as_deref(),
            })?;
            (arguments, initial_prompt, watcher, environment_removals)
        }
        None => {
            let provider::LaunchPlan {
                arguments,
                initial_prompt,
                watcher,
                environment_removals,
            } = provider::prepare_launch(
                provider,
                provider::LaunchContext {
                    bridge_executable: &executable,
                    directory,
                    workspace: &manifest.workspace,
                    title: &manifest.title,
                    prompt: &prompt,
                    yolo: manifest.yolo,
                    model: manifest.model.as_deref(),
                    effort: manifest.effort.as_deref(),
                },
            )?;
            (arguments, initial_prompt, watcher, environment_removals)
        }
    };

    let watcher = watcher.start(directory)?;
    let mut provider_command = process_env::provider_command(
        &manifest.provider_path,
        directory,
        arguments,
        environment_removals,
    )?;
    // The reopen ownership gate ran read-only before the source was claimed; the provider
    // grants no exclusive hold on the conversation, so it runs again here, after every other
    // preparation and immediately before the process exists. A holder that appeared in
    // between refuses under the same gate; the record carries the gate to the reopen command.
    if let Some(resumed_from) = &resumed_from
        && let Err(error) = provider::reopen(provider)
            .and_then(|support| support.verify_available(&resumed_from.provider_session_id))
    {
        let _ = watcher.stop();
        return Err(record_reopen_refusal(
            directory,
            ReopenGate::ProviderUnsupported,
            format!("{error:#}"),
        ));
    }
    configure_provider_launch(
        &mut provider_command,
        &manifest.workspace,
        directory,
        &manifest.id,
        &executable,
    );
    // Spawn and its durable identity are fenced against cancellation by the lifecycle
    // lock. Failed identity recording ends the child but stays delivery-uncertain: an
    // argument-delivered prompt could already have been consumed before cleanup.
    let child = super::spawn(
        &Store::open_unchecked(directory),
        &mut provider_command,
        |child| {
            record_provider_process(directory, &manifest.id, child)?;
            match initial_prompt {
                provider::InitialPromptDisposition::AcceptedAtSpawn => {
                    session::RecordStore::at(&prompt_path)
                        .remove_raw()
                        .context("failed to remove the accepted initial prompt")?;
                    update_status(directory, SessionState::Running, None, None)?;
                }
                provider::InitialPromptDisposition::AwaitingDelivery => {
                    update_status(directory, SessionState::AwaitingInitialInput, None, None)?;
                }
            }
            Ok(())
        },
    );
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = watcher.stop();
            return Err(error).with_context(|| {
                format!(
                    "failed to start {} at {}",
                    provider.as_str(),
                    manifest.provider_path.display()
                )
            });
        }
    };
    let status = child.wait();
    if let Ok(status) = &status {
        super::log(
            &Store::open_unchecked(directory),
            &format!("provider_exit_code={:?}", status.code()),
        );
    }
    watcher.stop()?;
    let status = status.with_context(|| {
        format!(
            "failed to start {} at {}",
            provider.as_str(),
            manifest.provider_path.display()
        )
    })?;
    if !status.success() {
        bail!("{} exited with {status}", provider.as_str());
    }
    Ok(())
}

pub(in crate::native) fn resolve_provider(provider: FirstPartyCli) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_provider_from_path(provider, &path)
}

pub(in crate::native) fn resolve_provider_from_path(
    provider: FirstPartyCli,
    path: &std::ffi::OsStr,
) -> Result<PathBuf> {
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        #[cfg(windows)]
        let names = [
            format!("{}.exe", provider.command()),
            format!("{}.ps1", provider.command()),
            format!("{}.cmd", provider.command()),
            format!("{}.bat", provider.command()),
        ];
        #[cfg(not(windows))]
        let names = [provider.command().to_owned()];
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() && is_executable(&candidate) {
                let canonical = candidate.canonicalize().with_context(|| {
                    format!(
                        "failed to canonicalize provider path {}",
                        candidate.display()
                    )
                })?;
                if canonical.is_file() && is_executable(&canonical) {
                    return Ok(canonical);
                }
            }
        }
    }
    bail!(
        "{} was not found on an absolute PATH entry",
        provider.command()
    )
}

pub(in crate::native) fn configure_provider_launch(
    command: &mut Command,
    workspace: &Path,
    directory: &Path,
    id: &str,
    executable: &Path,
) {
    command
        .current_dir(workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", id)
        .env("AGENT_BRIDGE_EXECUTABLE", executable);
    provider_stderr(command);
}

pub(in crate::native) fn provider_version_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut version = String::from_utf8_lossy(stdout).trim().to_owned();
    if version.is_empty() {
        version = String::from_utf8_lossy(stderr).trim().to_owned();
    }
    version
}

pub(in crate::native) fn version_probe_command(
    provider: FirstPartyCli,
    executable: &Path,
) -> Result<Command> {
    let mut command = process_env::provider_version_command(
        executable,
        provider::probe_environment_removals(provider),
    )?;
    command.arg("--version");
    Ok(command)
}

pub(in crate::native) fn check_provider_version(
    provider: FirstPartyCli,
    executable: &Path,
) -> Result<String> {
    check_provider_version_until(provider, executable, None)
}

pub(in crate::native) fn check_provider_version_until(
    provider: FirstPartyCli,
    executable: &Path,
    deadline: Option<Instant>,
) -> Result<String> {
    let mut command = version_probe_command(provider, executable)?;
    let label = format!("{} --version", executable.display());
    let output = match deadline {
        Some(deadline) => process_env::command_output_until(&mut command, deadline, &label),
        None => command
            .output()
            .with_context(|| format!("failed to query {label}")),
    }?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            executable.display(),
            output.status
        );
    }
    let version = provider_version_text(&output.stdout, &output.stderr);
    if !cli_version_is_supported(provider, &version)? {
        bail!(
            "{} is too old: found {version:?}, require >= {}",
            provider.as_str(),
            provider.minimum_version()
        );
    }
    Ok(version)
}

#[cfg(unix)]
pub(in crate::native) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(in crate::native) fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The shell redirects only Bridge's stderr. The provider keeps the original terminal
/// stderr, so logging does not turn its interactive surface into a pipe or a file.
pub(in crate::native) fn provider_stderr(command: &mut Command) {
    command.env_remove(STDERR_ENV);
    command.env_remove(STDOUT_ENV);
    #[cfg(unix)]
    if std::env::var(STDERR_ENV).as_deref() == Ok("3") {
        use std::os::fd::FromRawFd;
        // SAFETY: dup produces a new owned fd; a failed dup is not adopted.
        let fd = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
        if fd >= 0 {
            command.stderr(unsafe { File::from_raw_fd(fd) });
        }
    }
}

pub(in crate::native) fn restore_stdout() -> Result<()> {
    #[cfg(unix)]
    if std::env::var(STDOUT_ENV).as_deref() == Ok("4") {
        // SAFETY: the launch shell supplies fd 4 as its original stdout. This runs at
        // entry, before monitors/threads exist; provider stdout must remain the real TTY.
        if unsafe { libc::dup2(4, libc::STDOUT_FILENO) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to restore launch terminal stdout");
        }
    }
    Ok(())
}
