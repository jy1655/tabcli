use std::{ffi::OsString, path::Path, process::Command};

use anyhow::Result;
#[cfg(windows)]
use anyhow::{Context, bail};

#[cfg(windows)]
use super::{terminal, write_private};

pub(super) fn command(
    executable: &Path,
    _directory: &Path,
    arguments: Vec<OsString>,
) -> Result<Command> {
    #[cfg(windows)]
    if has_extension(executable, "ps1") {
        let mut command = powershell_file_command(executable)?;
        command.args(arguments);
        return Ok(command);
    }

    #[cfg(windows)]
    if has_extension(executable, "cmd") || has_extension(executable, "bat") {
        const FORWARDER: &str = r#"param(
  [Parameter(Mandatory=$true)][string]$Provider,
  [Parameter(ValueFromRemainingArguments=$true)][string[]]$ProviderArgs
)

function ConvertTo-NativeArgument([string]$Value) {
  $Builder = [System.Text.StringBuilder]::new()
  [void]$Builder.Append('"')
  $Backslashes = 0
  foreach ($Character in $Value.ToCharArray()) {
    if ($Character -eq '\') {
      $Backslashes++
      continue
    }
    if ($Character -eq '"') {
      [void]$Builder.Append(('\' * (($Backslashes * 2) + 1)))
      [void]$Builder.Append('"')
    } else {
      [void]$Builder.Append(('\' * $Backslashes))
      [void]$Builder.Append($Character)
    }
    $Backslashes = 0
  }
  [void]$Builder.Append(('\' * ($Backslashes * 2)))
  [void]$Builder.Append('"')
  $Builder.ToString()
}

$Names = [System.Collections.Generic.List[string]]::new()
for ($Index = 0; $Index -lt $ProviderArgs.Count; $Index++) {
  $Name = "AGENT_BRIDGE_BATCH_ARG_$Index"
  [Environment]::SetEnvironmentVariable($Name, (ConvertTo-NativeArgument $ProviderArgs[$Index]), 'Process')
  $Names.Add($Name)
}
$ArgumentLine = ($Names | ForEach-Object { "%$_%" }) -join ' '
$CommandLine = (ConvertTo-NativeArgument $Provider) + $(if ($ArgumentLine) { " $ArgumentLine" } else { '' })
$Cmd = Join-Path ([Environment]::GetFolderPath('System')) 'cmd.exe'
& $Cmd /d /v:off /s /c ('"' + $CommandLine + '"')
exit $LASTEXITCODE
"#;
        let forwarder = _directory.join("provider-launch.ps1");
        ensure_private_forwarder(&forwarder, FORWARDER.as_bytes())?;
        let mut command = powershell_file_command(&forwarder)?;
        command
            .arg(windows_command_path(executable))
            .args(arguments);
        return Ok(command);
    }

    let mut command = Command::new(executable);
    command.args(arguments);
    Ok(command)
}

#[cfg(windows)]
fn ensure_private_forwarder(path: &Path, expected: &[u8]) -> Result<()> {
    match std::fs::read(path) {
        Ok(existing) if existing == expected => return Ok(()),
        Ok(_) => bail!(
            "refusing to replace a mismatched provider forwarder {}",
            path.display()
        ),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| {
                format!("failed to inspect provider forwarder {}", path.display())
            });
        }
        Err(_) => {}
    }
    match write_private(path, expected) {
        Ok(()) => Ok(()),
        Err(write_error) => match std::fs::read(path) {
            Ok(existing) if existing == expected => Ok(()),
            _ => Err(write_error)
                .with_context(|| format!("failed to create provider forwarder {}", path.display())),
        },
    }
}

pub(super) fn version_command(executable: &Path) -> Result<Command> {
    #[cfg(windows)]
    if has_extension(executable, "ps1") {
        return powershell_file_command(executable);
    }
    Ok(Command::new(executable))
}

#[cfg(windows)]
fn powershell_file_command(script: &Path) -> Result<Command> {
    let mut command = Command::new(terminal::windows_powershell_executable()?);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    command.arg(windows_command_path(script));
    Ok(command)
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
