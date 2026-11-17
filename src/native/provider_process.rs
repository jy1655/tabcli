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
    let mut command = Command::new(windows_command_path(
        &terminal::windows_powershell_executable()?,
    ));
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
pub(in crate::native) fn windows_command_path(path: &Path) -> OsString {
    let value = path.as_os_str().to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return OsString::from(format!(r"\\{rest}"));
    }
    value
        .strip_prefix(r"\\?\")
        .map_or_else(|| path.as_os_str().to_owned(), OsString::from)
}

/// Linux refuses an exec when one argument or environment string, with its NUL, is longer
/// than MAX_ARG_STRLEN (32 pages), or when all of its strings and pointers exceed the stack
/// limit's share (fs/exec.c, `bprm_stack_limits`). Measured in bytes before the spawn, so
/// that a refusal is known to have run nothing.
#[cfg(target_os = "linux")]
pub(in crate::native) fn linux_exec_budget(command: &Command) -> anyhow::Result<()> {
    linux_exec_budget_with(
        command,
        std::env::vars_os(),
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) },
        unsafe { libc::sysconf(libc::_SC_ARG_MAX) },
    )
}

/// The kernel's total: its copy of the file name, then every argument (argv[0] is the
/// program) and environment string, each with its NUL, plus one pointer per argument (at
/// least one) and per environment string. The limit is a quarter of the stack limit, which
/// glibc reports as ARG_MAX, capped at three quarters of the 8 MiB default stack and never
/// below the kernel's ARG_MAX of 128 KiB (32 pages bound each string, not the total).
/// A `#!` entrypoint (the npm launchers of Codex and Pi) needs more: the kernel adds the
/// interpreter line and the script path, and an `env` interpreter execs the program again
/// with its own path; a fixed margin covers both.
#[cfg(target_os = "linux")]
fn linux_exec_budget_with(
    command: &Command,
    inherited: impl Iterator<Item = (OsString, OsString)>,
    page_size: libc::c_long,
    arg_max: libc::c_long,
) -> anyhow::Result<()> {
    use anyhow::{bail, ensure};
    ensure!(
        page_size > 0 && arg_max > 0,
        "could not read the Linux exec limits"
    );
    let string_limit = usize::try_from(page_size)? * 32;
    let limit = usize::try_from(arg_max)?.clamp(LINUX_ARG_MAX, LINUX_STACK_ARGUMENT_CAP);
    let mut environment: std::collections::BTreeMap<OsString, OsString> = inherited.collect();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => environment.insert(name.to_owned(), value.to_owned()),
            None => environment.remove(name),
        };
    }
    let program = command.get_program();
    let mut total = program.len() + 1;
    if script_entrypoint(program) {
        total += LINUX_SCRIPT_MARGIN;
    }
    let mut arguments = 0;
    for argument in std::iter::once(program).chain(command.get_args()) {
        let bytes = argument.len() + 1;
        if bytes > string_limit {
            bail!(
                "the provider command has an argument of {bytes} bytes; Linux refuses one longer than {string_limit} bytes. Shorten the prompt and pass long material by path in the workspace; nothing was sent"
            );
        }
        total += bytes;
        arguments += 1;
    }
    for (name, value) in &environment {
        let bytes = name.len() + value.len() + 2;
        if bytes > string_limit {
            bail!(
                "the provider environment variable {} is {bytes} bytes; Linux refuses one longer than {string_limit} bytes; nothing was sent",
                name.to_string_lossy()
            );
        }
        total += bytes;
    }
    total += (arguments.max(1) + environment.len()) * std::mem::size_of::<usize>();
    if total > limit {
        bail!(
            "the provider command and environment take {total} bytes; Linux refuses more than {limit} bytes. Shorten the prompt and pass long material by path in the workspace; nothing was sent"
        );
    }
    Ok(())
}

/// The kernel's ARG_MAX (include/uapi/linux/limits.h): the least total it allows.
#[cfg(target_os = "linux")]
const LINUX_ARG_MAX: usize = 128 * 1024;
/// Three quarters of `_STK_LIM` (8 MiB): the most total the kernel allows.
#[cfg(target_os = "linux")]
const LINUX_STACK_ARGUMENT_CAP: usize = 8 * 1024 * 1024 / 4 * 3;
/// Two paths of PATH_MAX (the script for the kernel, the program for an `env` interpreter)
/// and one interpreter line of BINPRM_BUF_SIZE, with their pointers.
#[cfg(target_os = "linux")]
const LINUX_SCRIPT_MARGIN: usize = 2 * 4096 + 256 + 4 * std::mem::size_of::<usize>();

// Only an entrypoint whose first bytes were read and are not `#!` goes without the margin:
// the kernel can run an execute-only script that this process cannot read.
#[cfg(target_os = "linux")]
fn script_entrypoint(program: &std::ffi::OsStr) -> bool {
    use std::io::Read;
    let mut start = [0; 2];
    std::fs::File::open(program)
        .and_then(|mut file| file.read_exact(&mut start))
        .map_or(true, |()| start == *b"#!")
}

#[cfg(all(test, target_os = "linux"))]
mod linux_exec_budget_tests {
    use super::*;

    // Refused: the exec failed with E2BIG. A child that ran must also have succeeded, so
    // that an interpreter's own failed exec is not taken for acceptance.
    fn kernel_refuses(command: &mut Command) -> bool {
        match command.status() {
            Ok(status) => {
                assert!(status.success(), "{status}");
                false
            }
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::E2BIG), "{error}");
                true
            }
        }
    }

    #[test]
    fn one_string_is_refused_exactly_where_the_kernel_refuses_it() {
        let limit = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize * 32;
        for (length, refused) in [(limit - 1, false), (limit, true)] {
            let mut command = Command::new("/bin/true");
            command.arg("x".repeat(length));
            assert_eq!(linux_exec_budget(&command).is_err(), refused, "{length}");
            assert_eq!(kernel_refuses(&mut command), refused, "{length}");
        }
    }

    // The arguments fill the total exactly, then by one byte more; the kernel decides both.
    // A script entrypoint is refused there too, and accepted a margin below.
    #[test]
    fn the_total_is_refused_exactly_where_the_kernel_refuses_it() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let script = |name: &str, line: &str, mode: u32| {
            let path = root.path().join(name);
            std::fs::write(&path, line).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path.to_str().unwrap().to_owned()
        };
        let shell = script("shell", "#!/bin/sh\n", 0o700);
        // An `env` interpreter execs the program again; its own exit status counts.
        let env = script("env", "#!/usr/bin/env sh\n", 0o700);
        // (entrypoint, bytes below the exact total, refused by the budget and by the kernel,
        // each at the boundary and one byte over it)
        for (program, under, budget, kernel) in [
            ("/bin/./true", 0, [false, true], [false, true]),
            (shell.as_str(), 0, [true, true], [true, true]),
            (
                shell.as_str(),
                LINUX_SCRIPT_MARGIN,
                [false, true],
                [false, false],
            ),
            (env.as_str(), 0, [true, true], [true, true]),
            (
                env.as_str(),
                LINUX_SCRIPT_MARGIN,
                [false, true],
                [false, false],
            ),
        ] {
            total_boundary(program, under, budget, kernel);
        }
        // An entrypoint this process cannot read keeps the margin.
        let unreadable = script("unreadable", "#!/bin/sh\n", 0o100);
        assert!(script_entrypoint(std::ffi::OsStr::new(&unreadable)));
        assert!(!script_entrypoint(std::ffi::OsStr::new("/bin/true")));
    }

    fn total_boundary(program: &str, under: usize, budget: [bool; 2], kernel: [bool; 2]) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let string_limit = page * 32;
        let limit = (unsafe { libc::sysconf(libc::_SC_ARG_MAX) } as usize)
            .clamp(LINUX_ARG_MAX, LINUX_STACK_ARGUMENT_CAP);
        let pointer = std::mem::size_of::<usize>();
        let environment: Vec<_> = std::env::vars_os().collect();
        let fixed = (program.len() + 1) * 2
            + under
            + environment
                .iter()
                .map(|(name, value)| name.len() + value.len() + 2)
                .sum::<usize>();
        let mut arguments: Vec<usize> = Vec::new();
        loop {
            let pointers = (arguments.len() + 2 + environment.len()) * pointer;
            let room =
                limit - fixed - arguments.iter().map(|bytes| bytes + 1).sum::<usize>() - pointers;
            if room > string_limit {
                arguments.push(string_limit - 1);
            } else {
                arguments.push(room - 1);
                break;
            }
        }
        for extra in [0, 1] {
            let mut command = Command::new(program);
            let last = arguments.len() - 1;
            for (index, bytes) in arguments.iter().enumerate() {
                command.arg("x".repeat(bytes + if index == last { extra } else { 0 }));
            }
            assert_eq!(
                linux_exec_budget(&command).is_err(),
                budget[extra],
                "{program} {extra}"
            );
            assert_eq!(
                kernel_refuses(&mut command),
                kernel[extra],
                "{program} {extra}"
            );
        }
    }

    // On 64 KiB pages one string may take 2 MiB, but the total keeps glibc's ARG_MAX.
    #[test]
    fn the_total_is_not_raised_to_the_string_limit_of_large_pages() {
        let mut command = Command::new("/bin/true");
        command.arg("x".repeat(300 * 1024));
        let error = linux_exec_budget_with(&command, std::iter::empty(), 64 * 1024, 256 * 1024)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Linux refuses more than 262144 bytes"),
            "{error}"
        );
        let error = linux_exec_budget_with(&command, std::iter::empty(), 4096, 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Linux refuses one longer than 131072"),
            "{error}"
        );
        command = Command::new("/bin/true");
        command.arg("x".repeat(100 * 1024));
        linux_exec_budget_with(&command, std::iter::empty(), 4096, 1).unwrap();
    }

    #[test]
    fn the_budget_counts_the_effective_environment() {
        let inherited = || {
            vec![
                (OsString::from("KEEP"), OsString::from("v")),
                (OsString::from("BIG"), OsString::from("y".repeat(200))),
            ]
            .into_iter()
        };
        let mut command = Command::new("/bin/true");
        command.env("SET", "z".repeat(150));
        let error = linux_exec_budget_with(&command, inherited(), 4, 1 << 20).unwrap_err();
        assert!(error.to_string().contains("BIG is 205 bytes"), "{error}");
        command.env_remove("BIG");
        let error = linux_exec_budget_with(&command, inherited(), 4, 1 << 20).unwrap_err();
        assert!(error.to_string().contains("SET is 155 bytes"), "{error}");
        command.env_remove("SET");
        linux_exec_budget_with(&command, inherited(), 4, 1 << 20).unwrap();
    }
}

/// First refused length, excluding the terminating NUL (CreateProcessW needs room for it).
#[cfg(windows)]
pub(super) const WINDOWS_COMMAND_LINE_LIMIT: usize = 32_767;

/// Length of std's Windows command line, excluding its terminating NUL.
/// Only for regular arguments with default quoting, as prepared by `command`:
/// no raw_arg, force_quotes, or direct batch-file launch.
#[cfg(windows)]
pub(super) fn windows_command_line_units(command: &Command) -> usize {
    use std::os::windows::ffi::OsStrExt;

    // std's make_command_line always quotes argv[0], without escaping it.
    let mut units = command.get_program().encode_wide().count() + 2;
    for argument in command.get_args() {
        // Match std's append_arg (Quote::Auto): quotes alone are escaped but
        // do not require surrounding quotes. Spaces, tabs and empty args do.
        let quoted = argument.is_empty()
            || argument
                .as_encoded_bytes()
                .iter()
                .any(|b| matches!(b, b' ' | b'\t'));
        units += 1 + if quoted { 2 } else { 0 };
        let mut backslashes = 0;
        for unit in argument.encode_wide() {
            units += 1;
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    units += backslashes + 1;
                }
                backslashes = 0;
            }
        }
        if quoted {
            units += backslashes;
        }
    }
    units
}

#[cfg(all(test, windows))]
mod command_line_tests {
    use super::*;

    #[test]
    fn windows_command_line_units_matches_std_quoting() {
        // Expected serialized arguments, independent of the length calculation.
        for (argument, serialized) in [
            ("plain", "plain"),
            ("two words", "\"two words\""),
            ("a\tb", "\"a\tb\""),
            ("", "\"\""),
            (r#"a"b"#, r#"a\"b"#),
            (r#"a\\"b"#, r#"a\\\\\"b"#),
            (r#"a \\"b\"#, r#""a \\\\\"b\\""#),
            (r"plain\", r"plain\"),
            ("\u{1f600}", "\u{1f600}"),
        ] {
            let mut command = Command::new("tool.exe");
            command.arg(argument);
            let expected = format!("\"tool.exe\" {serialized}").encode_utf16().count();
            assert_eq!(
                windows_command_line_units(&command),
                expected,
                "{argument:?}"
            );
        }
        let mut command = Command::new("C:\\some dir\\\u{1f600}.exe");
        command.args(["plain", "", "\u{1f600}"]);
        assert_eq!(
            windows_command_line_units(&command),
            "\"C:\\some dir\\\u{1f600}.exe\" plain \"\" \u{1f600}"
                .encode_utf16()
                .count()
        );
    }
}
