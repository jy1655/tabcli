//! Control of a managed Windows console from inside the session's own process tree.
use crate::native::NativeCommand;
use crate::native::session::primitives::require_valid_session_id;
#[cfg(windows)]
use crate::native::session::{Reader, RecordReader};
#[cfg(windows)]
use crate::native::terminal::ownership::verify_terminal_surface_ownership;
#[cfg(windows)]
use crate::native::terminal::{windows_console_handle_path, windows_console_root_never_ran};
#[cfg(windows)]
use crate::native::{provider, terminal};
#[cfg(windows)]
use agent_bridge::FirstPartyCli;
use anyhow::{Context, Result, bail};
use std::path::Path;
#[cfg(windows)]
use std::str::FromStr;
use std::time::Duration;

// Parsed on every target; paths and provider console input facts are resolved only
// after the managed session and its surface have been verified.
#[derive(Debug, PartialEq)]
pub(crate) enum ConsoleControlRequest {
    Screen,
    Send {
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

impl ConsoleControlRequest {
    #[cfg(windows)]
    pub(super) fn needs_only_bound_surface(&self) -> bool {
        matches!(self, Self::Close | Self::Window { .. })
    }
}

pub(super) fn parse(rest: &[String]) -> Result<NativeCommand> {
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
    let request = match (action.as_str(), tail) {
        ("send", [input, timeout_ms]) if valid_pending_prompt_name(input) => {
            ConsoleControlRequest::Send {
                input_name: input.clone(),
                timeout: timeout(timeout_ms)?,
            }
        }
        ("dialog", [input, timeout_ms]) if valid_pending_prompt_name(input) => {
            ConsoleControlRequest::Dialog {
                input_name: input.clone(),
                timeout: timeout(timeout_ms)?,
            }
        }
        ("window", [timeout_ms]) => ConsoleControlRequest::Window {
            timeout: timeout(timeout_ms)?,
        },
        ("close", []) => ConsoleControlRequest::Close,
        ("screen", []) => ConsoleControlRequest::Screen,
        _ => bail!("invalid native console control arguments"),
    };
    Ok(NativeCommand::ConsoleControl {
        request,
        id: id.clone(),
    })
}

fn valid_pending_prompt_name(value: &str) -> bool {
    value.starts_with("pending-prompt-")
        && value.ends_with(".txt")
        && value.len() <= 128
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

#[cfg(target_os = "windows")]
pub(super) fn run(request: ConsoleControlRequest, id: &str) -> Result<()> {
    let directory = Reader::session_directory(id)?;
    let manifest = Reader::open_unchecked(&directory).manifest()?;
    let session: terminal::TerminalSession =
        RecordReader::at(&windows_console_handle_path(&directory, &request)).json()?;
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("managed session is not owned by the Windows console transport");
    }
    // A window is looked up right after the console root was started, before the wrapper
    // has recorded itself as the owner. Like a close, it needs only the bound surface.
    if request.needs_only_bound_surface() {
        session.verify_managed_session(id)?;
    } else {
        verify_terminal_surface_ownership(&directory, id, &session)?;
    }
    let input_path = |name: &str| {
        Reader::open_unchecked(&directory)
            .private(name)
            .path()
            .to_owned()
    };
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let (submit_count, newline) = provider::initial_transport(provider)
        .facts()
        .surface
        .map_or((1, terminal::ConsoleNewline::LineFeed), |surface| {
            (surface.submit_count, surface.newline)
        });
    let records = terminal::WindowsSessionRecords {
        directory: &directory,
        root_never_ran: windows_console_root_never_ran(&directory),
    };
    let action = match request {
        ConsoleControlRequest::Screen => terminal::ConsoleControlAction::Screen,
        ConsoleControlRequest::Send {
            input_name,
            timeout,
        } => terminal::ConsoleControlAction::Send {
            input: input_path(&input_name),
            submit_count,
            newline,
            timeout,
        },
        ConsoleControlRequest::Dialog {
            input_name,
            timeout: _,
        } => terminal::ConsoleControlAction::Dialog {
            input: input_path(&input_name),
        },
        ConsoleControlRequest::Window { timeout } => {
            terminal::ConsoleControlAction::Window { timeout }
        }
        ConsoleControlRequest::Close => terminal::ConsoleControlAction::Close,
    };
    terminal::windows_console_control(action, &session, &records)
}

#[cfg(not(target_os = "windows"))]
pub(super) fn run(_request: ConsoleControlRequest, _id: &str) -> Result<()> {
    bail!("native Windows console control is only available on Windows")
}
