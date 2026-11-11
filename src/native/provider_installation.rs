//! The installed provider CLI: where its executable is, whether a path is a usable executable,
//! and what a `--version` probe says about the version. Each caller runs the probe with its
//! own executor and expresses the observation in its own vocabulary: the `ask`/`reopen`
//! preflight and the wrapper's re-check refuse through `check_provider_version_until`, and
//! `doctor` reports rows.
use crate::native::failure_kind::Unsupported;
use crate::native::{process_env, provider};
use agent_bridge::{FirstPartyCli, cli_version_is_supported};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    time::Instant,
};

pub(in crate::native) fn resolve_provider(provider: FirstPartyCli) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_provider_from_path(provider, &path)
}

// Only absolute entries are searched, and a candidate counts only when both its own path and
// its canonical path pass the validity test; the canonical path is returned.
pub(in crate::native) fn resolve_provider_from_path(
    provider: FirstPartyCli,
    path: &OsStr,
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
            if is_usable_executable(&candidate) {
                let canonical = candidate.canonicalize().with_context(|| {
                    format!(
                        "failed to canonicalize provider path {}",
                        candidate.display()
                    )
                })?;
                if is_usable_executable(&canonical) {
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

/// The validity test of a provider executable: an absolute path to a regular file that is
/// executable. A relative path is refused before the file system is examined.
pub(in crate::native) fn is_usable_executable(path: &Path) -> bool {
    path.is_absolute() && path.is_file() && is_executable(path)
}

// Reads only the permission bits, following symlinks; the validity test supplies the
// regular-file test.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// A provider executable path observed against the validity test.
#[derive(Debug, PartialEq)]
pub(in crate::native) enum Executable {
    /// Absolute, a regular file, and executable.
    Usable(PathBuf),
    /// Not absolute; the file system was not examined.
    Relative(PathBuf),
    /// Absolute, but missing, not a regular file, or not executable.
    Unusable(PathBuf),
}

impl Executable {
    pub(in crate::native) fn observe(path: PathBuf) -> Self {
        if is_usable_executable(&path) {
            Self::Usable(path)
        } else if !path.is_absolute() {
            Self::Relative(path)
        } else {
            Self::Unusable(path)
        }
    }
}

/// The version text of a `--version` probe: its trimmed stdout, or its trimmed stderr when
/// stdout is blank, both decoded lossily.
pub(in crate::native) fn provider_version_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut version = String::from_utf8_lossy(stdout).trim().to_owned();
    if version.is_empty() {
        version = String::from_utf8_lossy(stderr).trim().to_owned();
    }
    version
}

/// The verdict on a version text against the provider's minimum version.
#[derive(Debug)]
pub(in crate::native) enum VersionSupport {
    Supported,
    TooOld,
    /// No semantic version could be parsed from the text; the error says so.
    Unrecognized(anyhow::Error),
}

pub(in crate::native) fn version_support(provider: FirstPartyCli, text: &str) -> VersionSupport {
    match cli_version_is_supported(provider, text) {
        Ok(true) => VersionSupport::Supported,
        Ok(false) => VersionSupport::TooOld,
        Err(error) => VersionSupport::Unrecognized(error),
    }
}

/// What a `--version` probe that ran to its end says.
#[derive(Debug)]
pub(in crate::native) enum VersionReport {
    /// The probe exited unsuccessfully; its output is not read.
    Exited(ExitStatus),
    /// The probe exited successfully: its version text and that text's verdict.
    Read {
        text: String,
        support: VersionSupport,
    },
}

pub(in crate::native) fn read_version_probe(
    provider: FirstPartyCli,
    output: &Output,
) -> VersionReport {
    if !output.status.success() {
        return VersionReport::Exited(output.status);
    }
    let text = provider_version_text(&output.stdout, &output.stderr);
    let support = version_support(provider, &text);
    VersionReport::Read { text, support }
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

// The launch gate: a supported version returns its text; anything else refuses, and only a
// version below the minimum is `Unsupported`. With a deadline the probe runs through the
// bounded executor; without one it runs unbounded.
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
    match read_version_probe(provider, &output) {
        VersionReport::Exited(status) => {
            bail!("{} --version exited with {}", executable.display(), status)
        }
        VersionReport::Read {
            text,
            support: VersionSupport::Supported,
        } => Ok(text),
        VersionReport::Read {
            text,
            support: VersionSupport::TooOld,
        } => bail!(Unsupported::new(format!(
            "{} is too old: found {text:?}, require >= {}",
            provider.as_str(),
            provider.minimum_version()
        ))),
        VersionReport::Read {
            support: VersionSupport::Unrecognized(error),
            ..
        } => Err(error),
    }
}

#[cfg(test)]
mod tests;
