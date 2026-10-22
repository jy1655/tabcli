use crate::native::NativeCommand;
use crate::native::session::primitives::require_valid_session_id;
use anyhow::{Context, Result, bail};
use std::{num::NonZeroUsize, path::Path, time::Duration};

#[cfg(windows)]
use crate::native::{
    provider,
    session::{Reader, RecordReader},
    terminal::{self, ownership::verify_terminal_surface_ownership},
};
#[cfg(windows)]
use std::{ffi::OsString, path::PathBuf, str::FromStr};

#[cfg(windows)]
mod client;
#[cfg(windows)]
pub(in crate::native) use client::{Client, HelperProcess, WindowLookup, installed};

#[derive(Debug, PartialEq)]
pub(crate) struct ConsoleControlRequest {
    operation: Operation,
}

#[derive(Debug, PartialEq)]
enum Operation {
    Screen,
    Send {
        mode: SendMode,
        input_name: String,
        timeout: Duration,
    },
    Dialog {
        input_name: String,
        timeout: Duration,
    },
    Window {
        timeout: Duration,
    },
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SendMode {
    ProviderDefault,
    Submit(NonZeroUsize),
    TextOnly,
}

impl SendMode {
    fn from_override(count: Option<usize>) -> Self {
        match count {
            None => Self::ProviderDefault,
            Some(0) => Self::TextOnly,
            Some(count) => Self::Submit(NonZeroUsize::new(count).unwrap()),
        }
    }

    #[cfg(windows)]
    fn override_count(self) -> Option<usize> {
        match self {
            Self::ProviderDefault => None,
            Self::TextOnly => Some(0),
            Self::Submit(count) => Some(count.get()),
        }
    }
}

impl ConsoleControlRequest {
    #[cfg(windows)]
    fn needs_only_bound_surface(&self) -> bool {
        matches!(self.operation, Operation::Close | Operation::Window { .. })
    }

    #[cfg(windows)]
    fn is_close(&self) -> bool {
        matches!(self.operation, Operation::Close)
    }
}

pub(super) fn parse(rest: &[String]) -> Result<NativeCommand> {
    let (id, request) = decode_arguments(rest)?;
    Ok(NativeCommand::ConsoleControl { request, id })
}

fn decode_arguments(rest: &[String]) -> Result<(String, ConsoleControlRequest)> {
    let [action, id, tail @ ..] = rest else {
        bail!("native-console-control requires an action and managed session id");
    };
    require_valid_session_id(id)?;
    if !matches!(
        action.as_str(),
        "send" | "close" | "screen" | "dialog" | "window"
    ) {
        bail!("unsupported native console action: {action}");
    }
    let timeout = |timeout_ms: &String| -> Result<Duration> {
        let timeout_ms = timeout_ms
            .parse::<u64>()
            .context("invalid native console timeout")?;
        if timeout_ms == 0 {
            bail!("native console timeout must be positive");
        }
        Ok(Duration::from_millis(timeout_ms))
    };
    let operation = match (action.as_str(), tail) {
        ("send", [input, timeout_ms, count @ ..])
            if valid_pending_prompt_name(input) && count.len() <= 1 =>
        {
            Operation::Send {
                mode: SendMode::from_override(
                    count
                        .first()
                        .map(|value| value.parse::<usize>())
                        .transpose()
                        .context("invalid native console control arguments")?,
                ),
                input_name: input.clone(),
                timeout: timeout(timeout_ms)?,
            }
        }
        ("dialog", [input, timeout_ms]) if valid_pending_prompt_name(input) => Operation::Dialog {
            input_name: input.clone(),
            timeout: timeout(timeout_ms)?,
        },
        ("window", [timeout_ms]) => Operation::Window {
            timeout: timeout(timeout_ms)?,
        },
        ("close", []) => Operation::Close,
        ("screen", []) => Operation::Screen,
        _ => bail!("invalid native console control arguments"),
    };
    Ok((id.clone(), ConsoleControlRequest { operation }))
}

fn valid_pending_prompt_name(value: &str) -> bool {
    value.starts_with("pending-prompt-")
        && value.ends_with(".txt")
        && value.len() <= 128
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

#[cfg(windows)]
fn encode_arguments(id: &str, request: &ConsoleControlRequest) -> Vec<OsString> {
    let (action, input, timeout, count) = match &request.operation {
        Operation::Screen => ("screen", None, None, None),
        Operation::Close => ("close", None, None, None),
        Operation::Window { timeout } => ("window", None, Some(*timeout), None),
        Operation::Dialog {
            input_name,
            timeout,
        } => ("dialog", Some(input_name), Some(*timeout), None),
        Operation::Send {
            input_name,
            timeout,
            mode,
        } => (
            "send",
            Some(input_name),
            Some(*timeout),
            mode.override_count(),
        ),
    };
    let mut args = vec![action.into(), id.into()];
    if let Some(input) = input {
        args.push(input.into());
    }
    if let Some(timeout) = timeout {
        args.push(timeout.as_millis().to_string().into());
    }
    if let Some(count) = count {
        args.push(count.to_string().into());
    }
    args
}

#[cfg(windows)]
pub(in crate::native) enum ResolvedAction {
    Screen,
    Send {
        input: PathBuf,
        submit_count: usize,
        newline: terminal::ConsoleNewline,
        timeout: Duration,
    },
    Dialog {
        input: PathBuf,
    },
    Window {
        timeout: Duration,
    },
    Close,
}

#[cfg(windows)]
pub(super) fn run(request: ConsoleControlRequest, id: &str) -> Result<()> {
    run_with(
        request,
        id,
        verify_terminal_surface_ownership,
        terminal::windows_console_control,
    )
}

#[cfg(windows)]
fn run_with(
    request: ConsoleControlRequest,
    id: &str,
    verify_owner: impl FnOnce(&Path, &str, &terminal::TerminalSession) -> Result<()>,
    execute: impl FnOnce(
        ResolvedAction,
        &terminal::TerminalSession,
        &terminal::WindowsSessionRecords<'_>,
    ) -> Result<()>,
) -> Result<()> {
    let directory = Reader::session_directory(id)?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let session: terminal::TerminalSession = RecordReader::at(
        &terminal::windows_console_handle_path(&directory, request.is_close()),
    )
    .json()?;
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("managed session is not owned by the Windows console transport");
    }
    if request.needs_only_bound_surface() {
        session.verify_managed_session(id)?;
    } else {
        verify_owner(&directory, id, &session)?;
    }
    let provider =
        agent_bridge::FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let records = terminal::WindowsSessionRecords {
        directory: &directory,
        root_never_ran: terminal::windows_console_root_never_ran(&directory),
    };
    let action = resolve_action(request, &directory, provider);
    execute(action, &session, &records)
}

#[cfg(windows)]
fn resolve_action(
    request: ConsoleControlRequest,
    directory: &Path,
    provider: agent_bridge::FirstPartyCli,
) -> ResolvedAction {
    let input_path = |name: &str| {
        Reader::open_unchecked(directory)
            .private(name)
            .path()
            .to_owned()
    };
    let (submit_count, newline) = provider::initial_transport(provider)
        .facts()
        .surface
        .map_or((1, terminal::ConsoleNewline::LineFeed), |surface| {
            (surface.submit_count, surface.newline)
        });
    match request.operation {
        Operation::Screen => ResolvedAction::Screen,
        Operation::Send {
            mode,
            input_name,
            timeout,
        } => ResolvedAction::Send {
            input: input_path(&input_name),
            submit_count: mode.override_count().unwrap_or(submit_count),
            newline,
            timeout,
        },
        Operation::Dialog { input_name, .. } => ResolvedAction::Dialog {
            input: input_path(&input_name),
        },
        Operation::Window { timeout } => ResolvedAction::Window { timeout },
        Operation::Close => ResolvedAction::Close,
    }
}

pub(in crate::native) const SUBMIT_DELAYS_EXHAUSTED: &str =
    "Windows console submission delays do not fit inside the remaining turn timeout";
#[cfg(windows)]
pub(in crate::native) const STALE_BEFORE_INPUT: &str =
    "Windows console claim stale before any input";
#[cfg(windows)]
pub(in crate::native) const STALE_AFTER_INPUT: &str =
    "Windows console claim stale after input dispatch";
#[cfg(windows)]
pub(in crate::native) const MISSING_PROCESS: &str = "console process is no longer available";

#[cfg(windows)]
pub(in crate::native) fn helper_reports_missing(message: &str) -> bool {
    exact_helper_message(message, MISSING_PROCESS)
}

#[cfg(windows)]
pub(in crate::native) fn helper_proves_no_input(message: &str) -> bool {
    exact_helper_message(message, STALE_BEFORE_INPUT)
        || exact_helper_message(message, SUBMIT_DELAYS_EXHAUSTED)
}

#[cfg(windows)]
fn exact_helper_message(message: &str, expected: &str) -> bool {
    let message = message.trim();
    message == expected || message.strip_prefix("Error: ") == Some(expected)
}

#[cfg(windows)]
fn helper_failure_message(output: &std::process::Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        "Windows console control helper failed".to_owned()
    } else {
        message
    }
}

#[cfg(windows)]
fn classify_send_output(
    output: std::process::Output,
) -> std::result::Result<Vec<u8>, terminal::TerminalSendFailure> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let message = helper_failure_message(&output);
    let not_sent = helper_proves_no_input(&message);
    let error = anyhow::anyhow!(message);
    Err(if not_sent {
        terminal::TerminalSendFailure::not_sent(error)
    } else {
        terminal::TerminalSendFailure::delivery_uncertain(error)
    })
}

#[cfg(windows)]
#[derive(serde::Serialize, serde::Deserialize)]
pub(in crate::native) struct GuardedTextInput {
    pub(in crate::native) text: String,
    #[serde(default)]
    pub(in crate::native) expected_claim: Option<String>,
    #[serde(default)]
    pub(in crate::native) expires_unix_ms: Option<u128>,
}

#[cfg(windows)]
pub(in crate::native) fn decode_text_payload(payload: &[u8]) -> Result<GuardedTextInput> {
    serde_json::from_slice(payload).context("invalid text-only console payload")
}

#[cfg(windows)]
pub(in crate::native) fn decode_dialog_payload(
    payload: &[u8],
) -> Result<terminal::GuardedDialogInput> {
    Ok(serde_json::from_slice(payload)?)
}

#[cfg(windows)]
pub(in crate::native) fn write_text_count(
    output: &mut impl std::io::Write,
    count: usize,
) -> Result<()> {
    writeln!(
        output,
        "{}",
        serde_json::json!({"schema": 1, "written_chars": count})
    )?;
    Ok(())
}

#[cfg(windows)]
pub(in crate::native) fn parse_text_only_output(
    output: &[u8],
) -> std::result::Result<usize, terminal::TerminalSendFailure> {
    parse_text_only_count(output).map_err(terminal::TerminalSendFailure::delivery_uncertain)
}

#[cfg(windows)]
fn parse_text_only_count(output: &[u8]) -> Result<usize> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Written {
        schema: u32,
        written_chars: usize,
    }
    let line = std::str::from_utf8(output)?.trim();
    if line.lines().count() != 1 {
        bail!("expected one Windows console text-only result line");
    }
    let value: Written =
        serde_json::from_str(line).context("invalid Windows console text-only result")?;
    if value.schema != 1 {
        bail!("invalid Windows console text-only result schema");
    }
    Ok(value.written_chars)
}

#[cfg(windows)]
#[derive(Debug)]
pub(in crate::native) struct StaleDialogClaim;
#[cfg(windows)]
impl std::fmt::Display for StaleDialogClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stale claim at guarded submission")
    }
}
#[cfg(windows)]
impl std::error::Error for StaleDialogClaim {}

#[cfg(windows)]
fn decode_dialog_reply(output: &[u8]) -> Result<bool> {
    match String::from_utf8_lossy(output).trim() {
        "sent" => Ok(true),
        "changed" => Ok(false),
        "stale" => Err(StaleDialogClaim.into()),
        _ => bail!("unexpected managed console dialog result"),
    }
}

#[cfg(windows)]
#[derive(Debug, Eq, PartialEq)]
pub(in crate::native) enum DialogReply {
    Sent,
    Changed,
    Stale,
}

#[cfg(windows)]
pub(in crate::native) fn write_dialog_reply(
    output: &mut impl std::io::Write,
    reply: DialogReply,
) -> Result<()> {
    let message = match reply {
        DialogReply::Sent => "sent",
        DialogReply::Changed => "changed",
        DialogReply::Stale => "stale",
    };
    writeln!(output, "{message}")?;
    Ok(())
}

#[cfg(windows)]
pub(in crate::native) fn screen_output(screen: &str) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(screen)?))
}

#[cfg(windows)]
pub(in crate::native) fn window_output(window: isize) -> String {
    format!("{window}\n")
}

#[cfg(windows)]
fn decode_window(output: &[u8]) -> Option<isize> {
    String::from_utf8_lossy(output)
        .trim()
        .parse::<isize>()
        .ok()
        .filter(|window| *window != 0)
}

#[cfg(not(windows))]
pub(super) fn run(_request: ConsoleControlRequest, _id: &str) -> Result<()> {
    bail!("native Windows console control is only available on Windows")
}

#[cfg(test)]
mod tests;
#[cfg(all(test, windows))]
pub(in crate::native) use tests::with_in_process_helper;
