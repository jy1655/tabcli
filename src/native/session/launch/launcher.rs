//! The launcher operation of the launch protocol: the launch receipt, the bootstrap
//! command that runs the wrapper in the surface, the bound surface and the confirmed
//! startup. The bootstrap builders live beside it.
#[cfg(unix)]
use crate::native::session::launch::wrapper::{STDERR_ENV, STDOUT_ENV};
use crate::native::session::launch::{self, WRAPPER_EXIT_CODE_MARKER};
use crate::native::session::primitives::STATE_DIR_ENV;
use crate::native::session::turn::Claim;
use crate::native::session::{CoreRecord, Store};
#[cfg(unix)]
use crate::native::terminal::shell_quote;
use crate::native::{CreatedSession, terminal};
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::ffi::OsString;
use std::{path::Path, time::Instant};

// The start operation: from the launch receipt to the bound surface whose wrapper has
// confirmed its startup. The caller reports the address before it and completes consent,
// confirms the startup on the claim and delivers after it.
pub(in crate::native) fn start(
    created: &CreatedSession,
    terminal_kind: terminal::TerminalKind,
    initial_claim: &mut Claim,
    expected_claim_token: &str,
    deadline: Instant,
) -> Result<terminal::TerminalSession> {
    let launch_deadline = launch::begin(
        &Store::open_unchecked(&created.directory),
        expected_claim_token,
        deadline,
    )?;
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    let bridge_command = bridge_shell_command(
        &created.manifest.workspace,
        created
            .directory
            .parent()
            .context("session directory has no state root")?,
        &executable,
        &created.id,
    )?;
    let bridge_command =
        launch::install_script(&Store::open_unchecked(&created.directory), &bridge_command)?;
    initial_claim.fence_launch();
    let terminal_session = match terminal::open_bound_tab(
        terminal_kind,
        &bridge_command,
        &created.directory,
        launch_deadline,
        |session| {
            session.managed_session_id = Some(created.id.clone());
            Store::open_unchecked(&created.directory).write_terminal(session)
        },
        || {
            Store::open_unchecked(&created.directory)
                .record(CoreRecord::Terminal)
                .remove()
        },
    ) {
        Ok(session) => session,
        Err(error) => {
            let error = launch::terminal_failure(&Store::open_unchecked(&created.directory), error);
            return Err(error).with_context(|| {
                format!(
                    "failed to open {} surface for session {}",
                    terminal_kind.display_name(),
                    created.id
                )
            });
        }
    };
    launch::wait(
        &Store::open_unchecked(&created.directory),
        &terminal_session,
        launch_deadline,
    )?;
    Ok(terminal_session)
}

#[cfg(unix)]
pub(in crate::native) fn bridge_shell_command(
    workspace: &Path,
    state_root: &Path,
    executable: &Path,
    id: &str,
) -> Result<String> {
    for (value, field) in [
        (workspace.as_os_str(), "workspace"),
        (state_root.as_os_str(), "state root"),
        (executable.as_os_str(), "Agent Bridge executable"),
        (std::ffi::OsStr::new(id), "session id"),
    ] {
        validate_shell_command_component(value, field)?;
    }
    let log = shell_quote(state_root.join(id).join(launch::LOG).as_os_str());
    let location = format!("cd {} &&", shell_quote(workspace.as_os_str()));
    let invocation = format!(
        "{STATE_DIR_ENV}={} {} native-session {}",
        shell_quote(state_root.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(OsString::from(id).as_os_str())
    );
    let start = format!("{location} {invocation}");
    let logged_start = format!("{location} {STDERR_ENV}=3 {STDOUT_ENV}=4 {invocation}");
    Ok(format!(
        "if (umask 077; : >> {log}) 2>/dev/null; then {{ {logged_start}; bridge_status=$?; printf '{WRAPPER_EXIT_CODE_MARKER}%s\\n' \"$bridge_status\" >> {log}; }} 3>&2 4>&1 >> {log} 2>&1; else {start}; bridge_status=$?; fi; exit \"$bridge_status\""
    ))
}

#[cfg(windows)]
fn powershell_quote(value: &std::ffi::OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

#[cfg(windows)]
pub(in crate::native) fn bridge_shell_command(
    workspace: &Path,
    state_root: &Path,
    executable: &Path,
    id: &str,
) -> Result<String> {
    for (value, field) in [
        (workspace.as_os_str(), "workspace"),
        (state_root.as_os_str(), "state root"),
        (executable.as_os_str(), "Agent Bridge executable"),
        (std::ffi::OsStr::new(id), "session id"),
    ] {
        validate_shell_command_component(value, field)?;
    }
    let log = powershell_quote(state_root.join(id).join(launch::LOG).as_os_str());
    // The console launch passes this to PowerShell as one double-quoted `-Command`
    // argument and refuses a command that holds a double quote, so every string here is
    // single-quoted.
    Ok(format!(
        "$bridgeStatus = 1; try {{ Set-Location -LiteralPath {} -ErrorAction Stop; $env:{} = {}; & {} native-session {}; $bridgeStatus = $LASTEXITCODE }} catch {{ try {{ Add-Content -LiteralPath {log} -Value $_ -ErrorAction Stop }} catch {{}} }}; try {{ Add-Content -LiteralPath {log} -Value ('{WRAPPER_EXIT_CODE_MARKER}' + $bridgeStatus) -ErrorAction Stop }} catch {{}}; exit $bridgeStatus",
        powershell_quote(workspace.as_os_str()),
        STATE_DIR_ENV,
        powershell_quote(state_root.as_os_str()),
        powershell_quote(executable.as_os_str()),
        powershell_quote(std::ffi::OsStr::new(id)),
    ))
}

pub(in crate::native) fn validate_shell_command_component(
    value: &std::ffi::OsStr,
    field: &str,
) -> Result<()> {
    if let Some(character) = value
        .to_string_lossy()
        .chars()
        .find(|value| value.is_control())
    {
        bail!(
            "{field} contains terminal control U+{:04X}",
            u32::from(character)
        );
    }
    Ok(())
}
