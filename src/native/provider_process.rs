#[cfg(windows)]
use crate::native::session::{RecordReader, RecordStore};
use std::{
    ffi::OsString,
    path::Path,
    process::{Child, Command},
};

#[cfg(windows)]
use anyhow::bail;
use anyhow::{Context, Result};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    },
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    },
    System::Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
};

#[cfg(windows)]
use super::terminal;

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

pub(super) fn configure_process_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_SUSPENDED);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}

pub(super) struct ProviderProcessTree {
    #[cfg(unix)]
    process_group: i32,
    #[cfg(windows)]
    job: HANDLE,
}

impl ProviderProcessTree {
    pub(super) fn attach(child: &Child) -> Result<Self> {
        #[cfg(unix)]
        {
            let process_group = i32::try_from(child.id())
                .context("provider process id cannot identify its process group")?;
            Ok(Self { process_group })
        }
        #[cfg(windows)]
        {
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("failed to create provider containment job");
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                let error = std::io::Error::last_os_error();
                unsafe {
                    CloseHandle(job);
                }
                return Err(error).context("failed to configure provider containment job");
            }
            let assigned = unsafe {
                AssignProcessToJobObject(job, child.as_raw_handle().cast::<core::ffi::c_void>())
            };
            if assigned == 0 {
                let error = std::io::Error::last_os_error();
                unsafe {
                    CloseHandle(job);
                }
                return Err(error).context("failed to contain provider process tree");
            }
            Ok(Self { job })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    pub(super) fn resume(&self, child: &Child) -> Result<()> {
        #[cfg(windows)]
        {
            resume_suspended_process(child.id())
        }
        #[cfg(not(windows))]
        {
            let _ = (self, child);
            Ok(())
        }
    }

    pub(super) fn terminate(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.process_group, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            TerminateJobObject(self.job, 1);
        }
    }
}

#[cfg(windows)]
fn resume_suspended_process(pid: u32) -> Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .context("failed to enumerate the suspended provider thread");
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to inspect the suspended provider thread");
    }
    loop {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("failed to open the suspended provider thread");
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
            if previous == u32::MAX {
                return Err(std::io::Error::last_os_error())
                    .context("failed to resume the contained provider process");
            }
            if previous != 1 {
                bail!("contained provider process had unexpected suspension count {previous}");
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            break;
        }
    }
    bail!("suspended provider process has no owned primary thread")
}

#[cfg(windows)]
impl Drop for ProviderProcessTree {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.job);
        }
    }
}

#[cfg(windows)]
fn ensure_private_forwarder(path: &Path, expected: &[u8]) -> Result<()> {
    match RecordReader::at(path).raw_bytes() {
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
    match RecordStore::at(path).write_private(expected) {
        Ok(()) => Ok(()),
        Err(write_error) => match RecordReader::at(path).raw_bytes() {
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
