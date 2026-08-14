use std::{ffi::OsString, path::Path, process::Command};

use anyhow::Result;
#[cfg(windows)]
use anyhow::bail;

#[cfg(windows)]
use super::{terminal, write_private};

pub(super) fn command(
    executable: &Path,
    _directory: &Path,
    arguments: Vec<OsString>,
) -> Result<Command> {
    #[cfg(windows)]
    if has_extension(executable, "ps1") {
        let mut command = Command::new(terminal::windows_powershell_executable()?);
        command.args(["-NoLogo", "-NoProfile", "-File"]);
        command
            .arg(windows_command_path(executable))
            .args(arguments);
        return Ok(command);
    }

    #[cfg(windows)]
    if has_extension(executable, "cmd") || has_extension(executable, "bat") {
        if arguments
            .iter()
            .any(|argument| argument.to_string_lossy().contains('%'))
        {
            bail!(
                "Windows batch provider arguments cannot contain '%' without expansion; install an .exe or .ps1 shim"
            );
        }
        const FORWARDER: &str = "param(\n  [Parameter(Mandatory=$true)][string]$Provider,\n  [Parameter(ValueFromRemainingArguments=$true)][string[]]$ProviderArgs\n)\n& $Provider @ProviderArgs\nexit $LASTEXITCODE\n";
        let forwarder = _directory.join("provider-launch.ps1");
        write_private(&forwarder, FORWARDER.as_bytes())?;
        let mut command = Command::new(terminal::windows_powershell_executable()?);
        command.args(["-NoLogo", "-NoProfile", "-File"]);
        command
            .arg(forwarder)
            .arg(windows_command_path(executable))
            .args(arguments);
        return Ok(command);
    }

    let mut command = Command::new(executable);
    command.args(arguments);
    Ok(command)
}

pub(super) fn version_command(executable: &Path) -> Result<Command> {
    #[cfg(windows)]
    if has_extension(executable, "ps1") {
        let mut command = Command::new(terminal::windows_powershell_executable()?);
        command.args(["-NoLogo", "-NoProfile", "-File"]);
        command.arg(windows_command_path(executable));
        return Ok(command);
    }
    Ok(Command::new(executable))
}

#[cfg(windows)]
fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

#[cfg(windows)]
fn windows_command_path(path: &Path) -> OsString {
    let value = path.as_os_str().to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return OsString::from(format!(r"\\{rest}"));
    }
    value
        .strip_prefix(r"\\?\")
        .map_or_else(|| path.as_os_str().to_owned(), OsString::from)
}
