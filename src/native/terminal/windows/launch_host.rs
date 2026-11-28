//! The bound PowerShell handoff of a Windows launch host, the program that a Herdr Core
//! starts in a new terminal. Windows has no exec, so the host stays the terminal's program:
//! it waits for its exact binding and the pending launch, discards the startup input,
//! starts the console root (the same PowerShell command line as a managed console's root)
//! in its own console, and ends with the root's exit code. The root inherits the host's
//! console and the core's job object, so ending the terminal ends the whole session.
use super::super::TerminalKind;
use anyhow::{Context, Result};
use std::{os::windows::process::CommandExt, process::Child};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

/// Start the console root in this console. The helper command removes the core's inherited
/// caller markers. PowerShell 7 is found on the PATH that the core gives this process.
pub(in crate::native::terminal) fn start_root(command: &str, kind: TerminalKind) -> Result<Child> {
    (|| {
        let arguments = super::console_root_arguments(command)?;
        let powershell = super::powershell_executable()?;
        ignore_interrupts()?;
        crate::native::process_env::helper_command(&powershell)
            .raw_arg(arguments)
            .spawn()
            .context("PowerShell did not start")
    })()
    .with_context(|| {
        format!(
            "could not start PowerShell for the {} launch",
            kind.as_str()
        )
    })
}

/// Once the root has started, a wait error gives no evidence of a startup refusal.
pub(in crate::native::terminal) fn wait_for_root(mut root: Child) -> Result<()> {
    let status = root.wait().context("cannot wait for the PowerShell root")?;
    std::process::exit(status.code().unwrap_or(1))
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
