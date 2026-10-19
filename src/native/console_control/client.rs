use super::*;
use crate::native::process_env::CommandOutputFailure;
use crate::native::session::{Reader, RecordStore};
use crate::native::terminal::{
    CloseOutcome, TerminalSendFailure, TerminalSendResult, TerminalSession,
};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Threading::DETACHED_PROCESS;

pub(super) trait HelperProcess {
    fn output(
        &mut self,
        command: &mut Command,
        deadline: Instant,
        label: &str,
    ) -> std::result::Result<Output, CommandOutputFailure>;
}

struct ProcessHelper;

impl HelperProcess for ProcessHelper {
    fn output(
        &mut self,
        command: &mut Command,
        deadline: Instant,
        label: &str,
    ) -> std::result::Result<Output, CommandOutputFailure> {
        #[cfg(test)]
        if let Some(output) = super::tests::helper_output(command, deadline, label) {
            return output;
        }
        crate::native::process_env::command_output_until_classified(command, deadline, label)
    }
}

pub(in crate::native) struct WindowLookup {
    helper: Option<std::process::Child>,
}

impl WindowLookup {
    pub(in crate::native) fn start(session: &TerminalSession, timeout: Duration) -> Self {
        use std::os::windows::process::CommandExt;
        let helper = request_command(
            session,
            Operation::Window {
                timeout: wire_timeout(timeout),
            },
        )
        .ok()
        .and_then(|mut command| {
            command
                // The helper needs no console of its own, and must never be given a
                // window that would itself come to the front.
                .creation_flags(DETACHED_PROCESS)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        });
        Self { helper }
    }

    pub(in crate::native) fn poll(&mut self) -> Option<isize> {
        let status = self.helper.as_mut()?.try_wait().ok().flatten()?;
        let output = self.helper.take()?.wait_with_output().ok()?;
        if !status.success() {
            return None;
        }
        decode_window(&output.stdout)
    }
}

impl Drop for WindowLookup {
    fn drop(&mut self) {
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.kill();
        }
    }
}

pub(in crate::native) fn send_file(
    session: &TerminalSession,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let prompt_path = prompt_path
        .to_str()
        .context("prompt path is not valid UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    run_console_send_helper(session, prompt_path, deadline, SendMode::ProviderDefault).map(|_| ())
}

pub(in crate::native) fn send_file_text_only(
    session: &TerminalSession,
    prompt_path: &Path,
    expected_claim: &str,
    deadline: Instant,
) -> std::result::Result<usize, TerminalSendFailure> {
    let payload = (|| -> Result<_> {
        let input = GuardedTextInput {
            text: std::fs::read_to_string(prompt_path)?,
            expected_claim: Some(expected_claim.to_owned()),
            expires_unix_ms: Some(
                crate::native::session::primitives::unix_ms()
                    + deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
            ),
        };
        dialog_input_file(
            prompt_path.parent().context("missing payload directory")?,
            session,
            &input,
        )
    })()
    .map_err(TerminalSendFailure::not_sent)?;
    let path = payload
        .path()
        .to_str()
        .context("payload path is not UTF-8")
        .map_err(TerminalSendFailure::not_sent)?;
    let output = run_console_send_helper(session, path, deadline, SendMode::TextOnly)?;
    parse_text_only_output(&output)
}

pub(in crate::native) fn read_screen(
    session: &TerminalSession,
    deadline: Instant,
) -> Result<String> {
    let mut command = request_command(session, Operation::Screen)?;
    let output = ProcessHelper
        .output(&mut command, deadline, "managed console screen")
        .map_err(CommandOutputFailure::into_error)?;
    if !output.status.success() {
        bail!("{}", helper_failure_message(&output));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

pub(in crate::native) fn guarded_dialog_input(
    directory: &Path,
    session: &TerminalSession,
    input: &terminal::GuardedDialogInput,
    deadline: Instant,
) -> Result<bool> {
    let file = dialog_input_file(directory, session, input)?;
    let path = file.path().to_str().context("dialog path is not UTF-8")?;
    let (mut command, id) = helper_command(session)?;
    let input_name = input_name(path)?;
    let timeout = wire_timeout(deadline.saturating_duration_since(Instant::now()));
    command.args(encode_arguments(
        &id,
        &ConsoleControlRequest {
            operation: Operation::Dialog {
                input_name,
                timeout,
            },
        },
    ));
    let output = ProcessHelper
        .output(&mut command, deadline, "managed console dialog input")
        .map_err(CommandOutputFailure::into_error)?;
    if !output.status.success() {
        bail!("{}", helper_failure_message(&output));
    }
    decode_dialog_reply(&output.stdout)
}

pub(in crate::native) fn close_session_until(
    _directory: &Path,
    session: &TerminalSession,
    deadline: Instant,
) -> Result<CloseOutcome> {
    let mut command = request_command(session, Operation::Close)?;
    match run_console_helper_command(&mut command, deadline) {
        Ok(()) => Ok(CloseOutcome::Closed),
        Err(error) if helper_reports_missing(&error.to_string()) => Ok(CloseOutcome::Missing),
        Err(error) => Err(error),
    }
}

fn run_console_send_helper(
    session: &TerminalSession,
    input: &str,
    deadline: Instant,
    mode: SendMode,
) -> std::result::Result<Vec<u8>, TerminalSendFailure> {
    let (mut command, id) = helper_command(session).map_err(TerminalSendFailure::not_sent)?;
    let input_name = input_name(input).map_err(TerminalSendFailure::not_sent)?;
    let timeout = terminal::windows_budget::remaining_send_budget_at(deadline, Instant::now())
        .map_err(TerminalSendFailure::not_sent)?;
    let timeout_ms = u64::try_from(timeout.as_millis())
        .context("Windows console timeout is too large")
        .map_err(TerminalSendFailure::not_sent)?;
    let request = ConsoleControlRequest {
        operation: Operation::Send {
            input_name,
            timeout: Duration::from_millis(timeout_ms.max(1)),
            mode,
        },
    };
    command.args(encode_arguments(&id, &request));
    run_console_send_command(&mut command, deadline)
}

pub(super) fn run_console_send_command(
    command: &mut Command,
    deadline: Instant,
) -> std::result::Result<Vec<u8>, TerminalSendFailure> {
    run_console_send_with(&mut ProcessHelper, command, deadline)
}

pub(super) fn run_console_send_with(
    process: &mut impl HelperProcess,
    command: &mut Command,
    deadline: Instant,
) -> std::result::Result<Vec<u8>, TerminalSendFailure> {
    let output = process
        .output(command, deadline, "Windows console control helper")
        .map_err(|failure| {
            if failure.process_started() {
                TerminalSendFailure::delivery_uncertain(failure.into_error())
            } else {
                TerminalSendFailure::not_sent(failure.into_error())
            }
        })?;
    classify_send_output(output)
}

pub(super) fn run_console_helper_command(command: &mut Command, deadline: Instant) -> Result<()> {
    let output = ProcessHelper
        .output(command, deadline, "Windows console control helper")
        .map_err(CommandOutputFailure::into_error)?;
    if output.status.success() {
        return Ok(());
    }
    bail!("{}", helper_failure_message(&output))
}

fn helper_command(session: &TerminalSession) -> Result<(Command, String)> {
    let executable = std::env::current_exe().context("failed to locate the current executable")?;
    let id = session
        .managed_session_id
        .as_deref()
        .context("Windows console handle is missing its managed session binding")?;
    let mut command = crate::native::process_env::helper_command(executable);
    let state_dir_env = crate::native::session::primitives::STATE_DIR_ENV;
    if let Some(root) = std::env::var_os(state_dir_env) {
        command.env(state_dir_env, root);
    }
    command.arg("native-console-control");
    Ok((command, id.to_owned()))
}

pub(super) fn request_command(session: &TerminalSession, operation: Operation) -> Result<Command> {
    let (mut command, id) = helper_command(session)?;
    command.args(encode_arguments(&id, &ConsoleControlRequest { operation }));
    Ok(command)
}

fn input_name(input: &str) -> Result<String> {
    Ok(Path::new(input)
        .file_name()
        .context("prompt path has no file name")?
        .to_str()
        .context("prompt path is not valid UTF-8")?
        .to_owned())
}

fn wire_timeout(timeout: Duration) -> Duration {
    if timeout.as_millis() == 0 {
        Duration::from_millis(1)
    } else {
        timeout
    }
}
pub(super) fn dialog_input_file(
    directory: &Path,
    session: &TerminalSession,
    input: &impl serde::Serialize,
) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let id = session
        .managed_session_id
        .as_deref()
        .context("missing managed session identity")?;
    if directory.file_name().and_then(|name| name.to_str()) != Some(id) {
        bail!("managed session identity does not match the supplied session directory");
    }
    let supplied = Reader::session_directory_in(
        directory
            .parent()
            .context("session directory has no parent")?,
        id,
    )?;
    let helper = Reader::session_directory(id)?;
    if supplied.canonicalize()? != helper.canonicalize()? {
        bail!("supplied session directory disagrees with the console helper state root");
    }
    let mut file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(directory)?;
    RecordStore::set_file_private(file.as_file())?;
    file.write_all(&serde_json::to_vec(input)?)?;
    file.flush()?;
    Ok(file)
}
