//! The bound PowerShell handoff of a Windows launch host, the program that a Herdr Core
//! starts in a new terminal. Windows has no exec, so the host stays the terminal's program:
//! it waits for its exact binding and the pending launch, discards the startup input,
//! starts the console root (the same PowerShell command line as a managed console's root)
//! in its own console, and ends with the root's exit code. The root inherits the host's
//! console and the core's job object, so ending the terminal ends the whole session.
use super::super::bound_host::{BoundHostSpec, prepare_bound_launch};
use super::super::{TerminalKind, TerminalSession};
use crate::native::clock::SystemClock;
use crate::native::session::Store;
use crate::native::session::launch::BindingFacts;
use anyhow::{Context, Result};
use std::os::windows::process::CommandExt;
use std::path::Path;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

/// Runs the host. Returns only an error: once the root has started, the host ends with it.
pub(in crate::native::terminal) fn run(
    directory: &Path,
    command: &str,
    kind: TerminalKind,
    binding: &BindingFacts,
    validate_binding: impl FnOnce(&TerminalSession) -> Result<()>,
) -> Result<()> {
    let name = kind.as_str();
    let refused = |error: anyhow::Error| {
        // The terminal finishes with this process and keeps the message on its screen.
        crate::native::session::launch::log(
            &Store::open_unchecked(directory),
            &format!("{name}_host_refused: {error:#}"),
        );
        error
    };
    prepare_bound_launch(
        directory,
        &BoundHostSpec { name: binding.host },
        binding,
        &mut SystemClock,
        validate_binding,
    )
    .map_err(refused)?;
    let code = start_root(command)
        .with_context(|| format!("could not start PowerShell for the {name} launch"))
        .map_err(refused)?;
    std::process::exit(code)
}

/// Starts the console root in this console and waits for it. The core's environment can
/// carry another session's caller markers; the helper command removes them, as the Linux
/// host's bash does. PowerShell 7 is found on this process's PATH, which the core gives it.
fn start_root(command: &str) -> Result<i32> {
    let arguments = super::console_root_arguments(command)?;
    let powershell = super::powershell_executable()?;
    let powershell = crate::native::provider_process::windows_command_path(&powershell);
    ignore_interrupts()?;
    let status = crate::native::process_env::helper_command(&powershell)
        .raw_arg(arguments)
        .status()
        .context("PowerShell did not start")?;
    Ok(status.code().unwrap_or(1))
}

// A Ctrl+C or Ctrl+Break reaches every process of the console. The root decides what it
// does with one; the host must outlive its root, which it waits for. A handler routine is
// not inherited, unlike ignoring the events altogether, so the root keeps its own.
unsafe extern "system" fn keep_running(event: u32) -> windows_sys::core::BOOL {
    i32::from(event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT)
}

fn ignore_interrupts() -> Result<()> {
    if unsafe { SetConsoleCtrlHandler(Some(keep_running), 1) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot keep the launch host running through console interrupts");
    }
    Ok(())
}
