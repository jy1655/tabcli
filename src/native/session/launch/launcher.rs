//! The launcher operation of the launch protocol: the launch receipt, the bootstrap
//! command that runs the wrapper in the surface, the bound surface and the confirmed
//! startup. The bootstrap builders live beside it.
#[cfg(unix)]
use crate::native::session::launch::wrapper::{STDERR_ENV, STDOUT_ENV};
use crate::native::session::launch::{self, WRAPPER_EXIT_CODE_MARKER};
use crate::native::session::primitives::STATE_DIR_ENV;
use crate::native::session::turn::Claim;
use crate::native::session::{CoreRecord, Store};
use crate::native::session_start::launch_path_text;
#[cfg(unix)]
use crate::native::terminal::shell_quote;
use crate::native::{CreatedSession, terminal};
use anyhow::{Context, Result, bail};
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
    launch::host::initialize_for_launch(
        &Store::open_unchecked(&created.directory),
        expected_claim_token,
        terminal_kind,
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
    // The launch host's verdict on the session directory, reproduced before any Surface
    // exists: a refusal is a failure to open the Surface and settles as one, for this launch
    // only (`launch::terminal_failure`).
    let opened = terminal::refuse_before_launch(terminal_kind, &created.directory).and_then(|()| {
        terminal::open_bound_tab(
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
        )
    });
    let terminal_session = match opened {
        Ok(session) => session,
        Err(error) => {
            #[cfg(test)]
            open_failure_hook::run(&created.directory);
            let store = Store::open_unchecked(&created.directory);
            let error =
                launch::terminal_failure(&store, expected_claim_token, error, launch_deadline);
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
        expected_claim_token,
        launch_deadline,
    )?;
    Ok(terminal_session)
}

pub(in crate::native) fn bridge_shell_command(
    workspace: &Path,
    state_root: &Path,
    executable: &Path,
    id: &str,
) -> Result<String> {
    #[cfg(unix)]
    let quote = |value: &str| shell_quote(value.as_ref());
    #[cfg(windows)]
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let quote_path = |path: &Path, field: &str| -> Result<String> {
        let text = launch_path_text(path, field)?;
        validate_shell_command_component(text.as_ref(), field)?;
        Ok(quote(text))
    };
    let workspace = quote_path(workspace, "workspace")?;
    let log_path = state_root.join(id).join(launch::LOG);
    let state_root = quote_path(state_root, "state root")?;
    let executable = quote_path(executable, "TAB executable")?;
    validate_shell_command_component(id.as_ref(), "session id")?;
    let id = quote(id);
    let log = quote(launch_path_text(&log_path, "launch log")?);
    #[cfg(unix)]
    {
        let location = format!("cd {workspace} &&");
        let invocation = format!("{STATE_DIR_ENV}={state_root} {executable} native-session {id}");
        let start = format!("{location} {invocation}");
        let logged_start = format!("{location} {STDERR_ENV}=3 {STDOUT_ENV}=4 {invocation}");
        Ok(format!(
            "if (umask 077; : >> {log}) 2>/dev/null; then {{ {logged_start}; bridge_status=$?; printf '{WRAPPER_EXIT_CODE_MARKER}%s\\n' \"$bridge_status\" >> {log}; }} 3>&2 4>&1 >> {log} 2>&1; else {start}; bridge_status=$?; fi; exit \"$bridge_status\""
        ))
    }
    #[cfg(windows)]
    {
        // The console launch passes this to PowerShell as one double-quoted `-Command`
        // argument and refuses a command that holds a double quote, so every string here is
        // single-quoted.
        Ok(format!(
            "$bridgeStatus = 1; try {{ Set-Location -LiteralPath {workspace} -ErrorAction Stop; $env:{STATE_DIR_ENV} = {state_root}; & {executable} native-session {id}; $bridgeStatus = $LASTEXITCODE }} catch {{ try {{ Add-Content -LiteralPath {log} -Value $_ -ErrorAction Stop }} catch {{}} }}; try {{ Add-Content -LiteralPath {log} -Value ('{WRAPPER_EXIT_CODE_MARKER}' + $bridgeStatus) -ErrorAction Stop }} catch {{}}; exit $bridgeStatus",
        ))
    }
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

/// A test's step between a failure to open the Surface and its settlement, run once on this
/// thread: what another process does at that moment, such as a reader taking the lock.
#[cfg(test)]
pub(in crate::native) mod open_failure_hook {
    use std::{cell::RefCell, path::Path};

    type Hook = Box<dyn FnOnce(&Path)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native) fn set(hook: impl FnOnce(&Path) + 'static) {
        HOOK.with(|slot| assert!(slot.borrow_mut().replace(Box::new(hook)).is_none()));
    }

    pub(super) fn run(directory: &Path) {
        if let Some(hook) = HOOK.with(|slot| slot.borrow_mut().take()) {
            hook(directory);
        }
    }
}
