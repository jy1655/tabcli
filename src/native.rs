#[cfg(test)]
mod tests;

mod provider;
mod provider_process;
mod terminal;

use provider_process::{
    command as provider_process_command, version_command as provider_version_command,
};

use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(not(windows))]
use agent_bridge::process_is_alive;
use agent_bridge::{
    FirstPartyCli, checked_deadline_from, cli_version_is_supported, confirm_explicit_close,
    provider_effort_args, provider_launch_args, provider_model_args, terminal_safe_text,
    validate_terminal_input,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const STATE_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_STATE_DIR";
const SESSION_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_SESSION_DIR";
const DEFAULT_TIMEOUT_SECS: u64 = 900;
const SESSION_SCHEMA: u32 = 1;
const TURN_CLAIM_FILE: &str = "turn.claim";
const SESSION_OWNER_FILE: &str = "native-session.json";
const CLOSED_STATUS_FILE: &str = "closed.json";
const PI_HOOK_FAILURE_FILE: &str = "pi-hook-failure.json";
const TERMINAL_HANDLE_FILE: &str = "terminal.json";
const TERMINAL_CLOSING_FILE: &str = "terminal.closing.json";
const TERMINAL_TOMBSTONE_FILE: &str = "terminal.closed.json";
static TURN_CLAIM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Ask(AskRequest),
    Tell(TellRequest),
    Sessions {
        json: bool,
    },
    Close(CloseRequest),
    RunSession {
        id: String,
    },
    Hook {
        provider: FirstPartyCli,
        payload: Option<String>,
    },
    ConsoleControl {
        action: String,
        id: String,
        input_name: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct AskRequest {
    pub(crate) provider: FirstPartyCli,
    pub(crate) workspace: PathBuf,
    pub(crate) prompt: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) terminal: Option<terminal::TerminalKind>,
    pub(crate) yolo: bool,
    pub(crate) timeout: Duration,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

#[derive(Debug)]
pub(crate) struct TellRequest {
    id: String,
    prompt: String,
    timeout: Duration,
    detach: bool,
    json: bool,
}

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionManifest {
    schema: u32,
    id: String,
    provider: String,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    yolo: bool,
    created_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionStatus {
    state: String,
    updated_unix_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionEvent {
    provider: String,
    message: String,
    #[serde(default)]
    error: Option<String>,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    created_unix_ms: u128,
}

#[derive(Debug, Deserialize, Serialize)]
struct PiHookFailureSignal {
    error: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct NativeSessionOwner {
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_tty_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_microseconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_process_identity: Option<terminal::WindowsProcessIdentity>,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeProcessIdentity {
    pid: u32,
    terminal_tty_device: u64,
    process_start_seconds: u64,
    process_start_microseconds: u64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacProcBsdInfo {
    _flags: u32,
    _status: u32,
    _exit_status: u32,
    pid: u32,
    _parent_pid: u32,
    _uid: u32,
    _gid: u32,
    _real_uid: u32,
    _real_gid: u32,
    _saved_uid: u32,
    _saved_gid: u32,
    _reserved: u32,
    _command: [libc::c_char; 16],
    _name: [libc::c_char; 32],
    _open_files: u32,
    _process_group: u32,
    _job_control_count: u32,
    terminal_tty_device: u32,
    _terminal_process_group: u32,
    _nice: i32,
    process_start_seconds: u64,
    process_start_microseconds: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
}

struct CreatedSession {
    id: String,
    directory: PathBuf,
    manifest: SessionManifest,
}

struct SessionSpec {
    provider: FirstPartyCli,
    provider_path: PathBuf,
    provider_version: String,
    workspace: PathBuf,
    title: String,
    model: Option<String>,
    effort: Option<String>,
    yolo: bool,
    prompt: String,
}

pub(crate) fn is_command(value: &str) -> bool {
    matches!(
        value,
        "ask"
            | "tell"
            | "sessions"
            | "close-session"
            | "native-session"
            | "native-hook"
            | "native-console-control"
    )
}

pub(crate) fn parse_args<I, S>(args: I) -> Result<NativeCommand>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect::<Vec<_>>();
    let (command, rest) = args.split_first().context("native command is required")?;
    match command.as_str() {
        "ask" => parse_ask(rest),
        "tell" => parse_tell(rest),
        "sessions" => parse_sessions(rest),
        "close-session" => parse_close(rest),
        "native-session" => {
            let id = one_positional(rest, "native-session requires one session id")?;
            require_valid_session_id(id)?;
            Ok(NativeCommand::RunSession { id: id.to_owned() })
        }
        "native-hook" => parse_hook(rest),
        "native-console-control" => {
            let [action, id, tail @ ..] = rest else {
                bail!("native-console-control requires an action and managed session id");
            };
            require_valid_session_id(id)?;
            if !matches!(action.as_str(), "send" | "close") {
                bail!("unsupported native console action: {action}");
            }
            let input_name = match (action.as_str(), tail) {
                ("send", [input]) if valid_pending_prompt_name(input) => Some(input.clone()),
                ("close", []) => None,
                _ => bail!("invalid native console control arguments"),
            };
            Ok(NativeCommand::ConsoleControl {
                action: action.clone(),
                id: id.clone(),
                input_name,
            })
        }
        _ => bail!("unknown native command: {command}"),
    }
}

fn parse_ask(args: &[String]) -> Result<NativeCommand> {
    let (provider, options) = args
        .split_first()
        .context("ask requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    let mut workspace = None;
    let mut prompt = None;
    let mut title = None;
    let mut model = None;
    let mut effort = None;
    let mut terminal = None;
    let mut yolo = false;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(options, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--title" => set_once(
                &mut title,
                option_value(options, &mut index, "--title")?.to_owned(),
                "--title",
            )?,
            "--model" => set_once(
                &mut model,
                option_value(options, &mut index, "--model")?.to_owned(),
                "--model",
            )?,
            "--effort" => set_once(
                &mut effort,
                option_value(options, &mut index, "--effort")?.to_owned(),
                "--effort",
            )?,
            "--terminal" => set_once(
                &mut terminal,
                terminal::TerminalKind::from_str(option_value(options, &mut index, "--terminal")?)
                    .map_err(anyhow::Error::msg)?,
                "--terminal",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--yolo" => set_flag_once(&mut yolo, "--yolo")?,
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown ask option: {option}"),
        }
        index += 1;
    }
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    let prompt = prompt.context("ask requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    if model.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--model cannot be empty");
    }
    if effort.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("--effort cannot be empty");
    }
    Ok(NativeCommand::Ask(AskRequest {
        provider,
        workspace,
        prompt,
        title,
        model,
        effort,
        terminal,
        yolo,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_tell(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args.split_first().context("tell requires one session id")?;
    require_valid_session_id(id)?;
    let mut prompt = None;
    let mut timeout = None;
    let mut detach = false;
    let mut json = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--prompt" => set_once(
                &mut prompt,
                option_value(options, &mut index, "--prompt")?.to_owned(),
                "--prompt",
            )?,
            "--timeout-secs" => {
                let value = option_value(options, &mut index, "--timeout-secs")?;
                set_once(&mut timeout, parse_timeout(value)?, "--timeout-secs")?;
            }
            "--detach" => set_flag_once(&mut detach, "--detach")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown tell option: {option}"),
        }
        index += 1;
    }
    let prompt = prompt.context("tell requires --prompt <text>")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
    Ok(NativeCommand::Tell(TellRequest {
        id: id.to_owned(),
        prompt,
        timeout: timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        detach,
        json,
    }))
}

fn parse_sessions(args: &[String]) -> Result<NativeCommand> {
    match args {
        [] => Ok(NativeCommand::Sessions { json: false }),
        [option] if option == "--json" => Ok(NativeCommand::Sessions { json: true }),
        _ => bail!("sessions accepts only --json"),
    }
}

fn parse_close(args: &[String]) -> Result<NativeCommand> {
    let (id, options) = args
        .split_first()
        .context("close-session requires one session id")?;
    require_valid_session_id(id)?;
    let mut explicit = false;
    let mut json = false;
    for option in options {
        match option.as_str() {
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            _ => bail!("unknown close-session option: {option}"),
        }
    }
    confirm_explicit_close(explicit)?;
    Ok(NativeCommand::Close(CloseRequest {
        id: id.to_owned(),
        explicit,
        json,
    }))
}

fn parse_hook(args: &[String]) -> Result<NativeCommand> {
    let (provider, payload) = args
        .split_first()
        .context("native-hook requires codex, claude, agy, or pi")?;
    let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
    if payload.len() > 1 {
        bail!("native-hook accepts at most one payload argument");
    }
    Ok(NativeCommand::Hook {
        provider,
        payload: payload.first().cloned(),
    })
}

fn one_positional<'a>(args: &'a [String], message: &str) -> Result<&'a str> {
    match args {
        [value] => Ok(value),
        _ => bail!("{message}"),
    }
}

fn option_value<'a>(args: &'a [String], index: &mut usize, option: &str) -> Result<&'a str> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        bail!("{option} may only be specified once");
    }
    Ok(())
}

fn set_flag_once(flag: &mut bool, option: &str) -> Result<()> {
    if *flag {
        bail!("{option} may only be specified once");
    }
    *flag = true;
    Ok(())
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("invalid timeout: {value}"))?;
    if seconds == 0 {
        bail!("timeout must be greater than zero");
    }
    let timeout = Duration::from_secs(seconds);
    checked_deadline_from(Instant::now(), timeout)?;
    Ok(timeout)
}

pub(crate) fn valid_session_id(value: &str) -> bool {
    value.starts_with("session-")
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn require_valid_session_id(value: &str) -> Result<()> {
    if !valid_session_id(value) {
        bail!("invalid Agent Bridge session id: {value:?}");
    }
    Ok(())
}

pub(crate) fn run(command: NativeCommand) -> Result<()> {
    match command {
        NativeCommand::Ask(request) => run_ask(request),
        NativeCommand::Tell(request) => run_tell(request),
        NativeCommand::Sessions { json } => run_sessions(json),
        NativeCommand::Close(request) => run_close(request),
        NativeCommand::RunSession { id } => run_session(&id),
        NativeCommand::Hook { provider, payload } => run_hook(provider, payload.as_deref()),
        NativeCommand::ConsoleControl {
            action,
            id,
            input_name,
        } => run_windows_console_control(&action, &id, input_name.as_deref()),
    }
}

fn valid_pending_prompt_name(value: &str) -> bool {
    value.starts_with("pending-prompt-")
        && value.ends_with(".txt")
        && value.len() <= 128
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

#[cfg(target_os = "windows")]
fn run_windows_console_control(action: &str, id: &str, input_name: Option<&str>) -> Result<()> {
    let directory = session_directory(id)?;
    let manifest = read_manifest(&directory)?;
    let session: terminal::TerminalSession =
        read_json(&windows_console_handle_path(&directory, action))?;
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("managed session is not owned by the Windows console transport");
    }
    verify_terminal_surface_ownership(&directory, id, &session)?;
    let input_path = input_name.map(|name| directory.join(name));
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let submit_count = usize::from(provider == FirstPartyCli::Codex) + 1;
    terminal::windows_console_control(action, &session, input_path.as_deref(), submit_count)
}

#[cfg(windows)]
fn windows_console_handle_path(directory: &Path, action: &str) -> PathBuf {
    let closing = directory.join(TERMINAL_CLOSING_FILE);
    if action == "close" && closing.is_file() {
        closing
    } else {
        directory.join(TERMINAL_HANDLE_FILE)
    }
}

#[cfg(not(target_os = "windows"))]
fn run_windows_console_control(_action: &str, _id: &str, _input_name: Option<&str>) -> Result<()> {
    bail!("native Windows console control is only available on Windows")
}

fn run_ask(request: AskRequest) -> Result<()> {
    let terminal_kind = terminal::select(request.terminal)?;
    let workspace = request.workspace.canonicalize().with_context(|| {
        format!(
            "workspace does not exist or cannot be resolved: {}",
            request.workspace.display()
        )
    })?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let provider_path = resolve_provider(request.provider)?;
    let provider_version = check_provider_version(request.provider, &provider_path)?;
    let requested_title = request.title.unwrap_or_else(|| {
        let workspace_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        format!("{} · {workspace_name}", request.provider.as_str())
    });
    let title = sanitize_title(&requested_title)?;
    let created = create_session(SessionSpec {
        provider: request.provider,
        provider_path,
        provider_version,
        workspace,
        title,
        model: request.model,
        effort: request.effort,
        yolo: request.yolo,
        prompt: native_delegation_prompt(&delegation_source(), &request.prompt),
    })?;
    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let bridge_command = bridge_shell_command(
        &created.manifest.workspace,
        created
            .directory
            .parent()
            .context("session directory has no state root")?,
        &executable,
        &created.id,
    )?;
    let mut terminal_session = match terminal::open_tab(terminal_kind, &bridge_command) {
        Ok(session) => session,
        Err(error) => {
            let _ = fs::remove_file(created.directory.join("initial-prompt.txt"));
            let _ = update_status(
                &created.directory,
                "failed",
                None,
                Some(format!("{error:#}")),
            );
            return Err(error).with_context(|| {
                format!(
                    "failed to open {} surface for session {}",
                    terminal_kind.display_name(),
                    created.id
                )
            });
        }
    };
    terminal_session.managed_session_id = Some(created.id.clone());
    write_json_atomic(&created.directory.join("terminal.json"), &terminal_session)?;

    if request.detach {
        return emit_session_result(
            request.json,
            &created.id,
            &terminal_session,
            request.provider,
            None,
        );
    }

    let event = wait_for_event(&created.directory, 0, request.timeout).with_context(|| {
        format!(
            "session {} remains open in {}; use `agent-bridge sessions` to inspect it",
            created.id,
            terminal_session.kind.display_name()
        )
    })?;
    emit_session_result(
        request.json,
        &created.id,
        &terminal_session,
        request.provider,
        Some(&event),
    )
}

fn verify_terminal_surface_ownership(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    session.verify_managed_session(expected_session_id)?;
    if session.kind == terminal::TerminalKind::AppleTerminal {
        verify_apple_terminal_owner(directory, expected_session_id, session)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_apple_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner_text = read_regular_text_if_present(&owner_path)?
        .with_context(|| "Terminal.app ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    let live = live_native_process_identity(owner.pid)?;
    let surface_tty_device = terminal_tty_device(Path::new(&session.id))?;
    verify_terminal_owner_attestation(
        expected_session_id,
        session,
        &owner,
        &live,
        surface_tty_device,
    )
}

#[cfg(not(target_os = "macos"))]
fn verify_apple_terminal_owner(
    _directory: &Path,
    _expected_session_id: &str,
    _session: &terminal::TerminalSession,
) -> Result<()> {
    bail!("Terminal.app ownership proof is only available on macOS")
}

#[cfg(any(target_os = "macos", test))]
fn verify_terminal_owner_attestation(
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
    surface_tty_device: u64,
) -> Result<()> {
    if session.kind != terminal::TerminalKind::AppleTerminal {
        bail!("native-session TTY attestation is only valid for Terminal.app")
    }
    session.verify_managed_session(expected_session_id)?;
    if session.window_id.as_deref().is_none_or(str::is_empty) {
        bail!("Terminal.app session record is missing its dedicated window id")
    }
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    if owner.terminal_tty.as_deref() != Some(session.id.as_str()) {
        bail!("native-session owner is attached to a different terminal TTY")
    }
    if owner.pid != live.pid {
        bail!("native-session owner PID no longer identifies the live process")
    }
    let owner_tty_device = owner
        .terminal_tty_device
        .context("native-session owner is missing its controlling TTY device")?;
    if owner_tty_device != live.terminal_tty_device || owner_tty_device != surface_tty_device {
        bail!("Terminal.app TTY no longer belongs to the native-session owner")
    }
    let owner_start_seconds = owner
        .process_start_seconds
        .context("native-session owner is missing its process start time")?;
    let owner_start_microseconds = owner
        .process_start_microseconds
        .context("native-session owner is missing its process start time")?;
    if owner_start_seconds != live.process_start_seconds
        || owner_start_microseconds != live.process_start_microseconds
    {
        bail!("native-session owner PID was reused by another process")
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn current_native_session_owner(session_id: &str) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    let live = live_native_process_identity(pid)?;
    let terminal_tty = current_terminal_tty()?;
    let terminal_tty_device = terminal_tty_device(Path::new(&terminal_tty))?;
    if live.terminal_tty_device != terminal_tty_device {
        bail!("native-session process is not attached to its reported terminal TTY")
    }
    Ok(NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        terminal_tty: Some(terminal_tty),
        terminal_tty_device: Some(terminal_tty_device),
        process_start_seconds: Some(live.process_start_seconds),
        process_start_microseconds: Some(live.process_start_microseconds),
        windows_process_identity: None,
    })
}

#[cfg(windows)]
fn current_native_session_owner(session_id: &str) -> Result<NativeSessionOwner> {
    let pid = std::process::id();
    Ok(NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        windows_process_identity: Some(terminal::windows_process_identity(pid)?),
        ..NativeSessionOwner::default()
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
fn current_native_session_owner(session_id: &str) -> Result<NativeSessionOwner> {
    Ok(NativeSessionOwner {
        pid: std::process::id(),
        managed_session_id: Some(session_id.to_owned()),
        ..NativeSessionOwner::default()
    })
}

#[cfg(target_os = "macos")]
fn current_terminal_tty() -> Result<String> {
    let mut buffer = [0 as libc::c_char; libc::PATH_MAX as usize];
    let error = unsafe { libc::ttyname_r(libc::STDIN_FILENO, buffer.as_mut_ptr(), buffer.len()) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error))
            .context("failed to resolve native-session controlling TTY");
    }
    let tty = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .context("native-session controlling TTY is not valid UTF-8")?;
    Ok(tty.to_owned())
}

#[cfg(target_os = "macos")]
fn terminal_tty_device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect terminal TTY {}", path.display()))?;
    if !metadata.file_type().is_char_device() {
        bail!("terminal TTY is not a character device: {}", path.display())
    }
    Ok(metadata.rdev())
}

#[cfg(target_os = "macos")]
fn live_native_process_identity(pid: u32) -> Result<NativeProcessIdentity> {
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    let pid_value = libc::c_int::try_from(pid).context("native-session PID is out of range")?;
    let buffer_size = libc::c_int::try_from(std::mem::size_of::<MacProcBsdInfo>())
        .context("macOS process-info structure is too large")?;
    let mut info = std::mem::MaybeUninit::<MacProcBsdInfo>::zeroed();
    let returned = unsafe {
        proc_pidinfo(
            pid_value,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if returned != buffer_size {
        if returned <= 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to inspect native-session process {pid}"));
        }
        bail!("macOS returned an incomplete identity for native-session process {pid}")
    }
    let info = unsafe { info.assume_init() };
    if info.pid != pid {
        bail!("macOS returned the wrong native-session process identity")
    }
    if info.terminal_tty_device == u32::MAX {
        bail!("native-session process has no controlling TTY")
    }
    Ok(NativeProcessIdentity {
        pid,
        terminal_tty_device: u64::from(info.terminal_tty_device),
        process_start_seconds: info.process_start_seconds,
        process_start_microseconds: info.process_start_microseconds,
    })
}

fn run_tell(request: TellRequest) -> Result<()> {
    let directory = session_directory(&request.id)?;
    repair_dead_native_owner(&directory)?;
    let claim = acquire_turn_claim(&directory)?;
    let manifest = read_manifest(&directory)?;
    let terminal_session: terminal::TerminalSession = read_json(&directory.join("terminal.json"))?;
    verify_terminal_surface_ownership(&directory, &request.id, &terminal_session)?;
    let baseline = event_paths(&directory)?.len();
    let previous_state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&previous_state) {
        bail!(
            "session {} is {previous_state}; tell requires the ready state",
            request.id
        );
    }
    update_status(&directory, "working", None, None)?;

    let mut prompt_file = tempfile::Builder::new()
        .prefix("pending-prompt-")
        .suffix(".txt")
        .tempfile_in(&directory)?;
    set_private_file_permissions(prompt_file.as_file())?;
    let prompt = native_delegation_prompt(&delegation_source(), &request.prompt);
    prompt_file.write_all(&terminal_paste_bytes(&prompt))?;
    prompt_file.flush()?;
    if let Err(error) = terminal::send_file(&terminal_session, prompt_file.path()) {
        let _ = update_status(
            &directory,
            &previous_state,
            None,
            Some(format!("{error:#}")),
        );
        return Err(error).with_context(|| {
            format!(
                "failed to type into visible {} session {}",
                terminal_session.kind.display_name(),
                request.id
            )
        });
    }
    claim.retain();

    if request.detach {
        return emit_session_result(
            request.json,
            &request.id,
            &terminal_session,
            FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
            None,
        );
    }
    let event = wait_for_event(&directory, baseline, request.timeout).with_context(|| {
        format!(
            "session {} remains open in {}; the requested turn did not report completion",
            request.id,
            terminal_session.kind.display_name()
        )
    })?;
    emit_session_result(
        request.json,
        &request.id,
        &terminal_session,
        FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?,
        Some(&event),
    )
}

fn run_sessions(json: bool) -> Result<()> {
    let root = state_root()?;
    let mut sessions = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_session_id(&id) {
                continue;
            }
            let directory = entry.path();
            let _ = repair_dead_native_owner(&directory);
            let Ok(manifest) = read_manifest(&directory) else {
                continue;
            };
            let status = read_json::<SessionStatus>(&directory.join("status.json")).ok();
            let terminal =
                read_json::<terminal::TerminalSession>(&directory.join("terminal.json")).ok();
            sessions.push(serde_json::json!({
                "id": manifest.id,
                "provider": manifest.provider,
                "workspace": manifest.workspace,
                "title": manifest.title,
                "yolo": manifest.yolo,
                "state": status.as_ref().map(|value| value.state.as_str()).unwrap_or("unknown"),
                "terminal": terminal.as_ref().map(|value| value.kind.as_str()),
                "terminal_session_id": terminal.as_ref().map(|value| value.id.as_str()),
                "terminal_tab_id": terminal.as_ref().and_then(|value| value.tab_id.as_deref()),
                "terminal_window_id": terminal.as_ref().and_then(|value| value.window_id.as_deref()),
                "iterm_session_id": terminal.as_ref()
                    .filter(|value| value.kind == terminal::TerminalKind::Iterm2)
                    .map(|value| value.id.as_str()),
                "results": event_paths(&directory).map(|paths| paths.len()).unwrap_or(0),
            }));
        }
    }
    sessions.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else if sessions.is_empty() {
        println!("no native Agent Bridge sessions");
    } else {
        for session in sessions {
            println!(
                "{}\t{}\t{}\t{}\tterminal={}\tyolo={}\t{} result(s)",
                session["id"].as_str().unwrap_or("?"),
                terminal_safe_text(session["state"].as_str().unwrap_or("unknown"), false),
                terminal_safe_text(session["provider"].as_str().unwrap_or("?"), false),
                terminal_safe_text(session["workspace"].as_str().unwrap_or("?"), false),
                session["terminal"].as_str().unwrap_or("unknown"),
                session["yolo"].as_bool().unwrap_or(false),
                session["results"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn run_close(request: CloseRequest) -> Result<()> {
    confirm_explicit_close(request.explicit)?;
    let directory = session_directory(&request.id)?;
    repair_dead_native_owner(&directory)?;
    close_session_state(&directory, |session| {
        verify_terminal_surface_ownership(&directory, &request.id, session)?;
        terminal::close_session(session)
    })
    .with_context(|| format!("failed to close visible terminal session {}", request.id))?;
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": request.id,
                "closed": true,
            }))?
        );
    } else {
        println!("closed {}", request.id);
    }
    Ok(())
}

fn close_session_state<F>(directory: &Path, mut close_terminal: F) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if status.state == "closed" {
        let consume_result = consume_terminal_handle(directory, None);
        let close_result = mark_session_closed(directory, None);
        consume_result?;
        return close_result;
    }

    let terminal_path = directory.join(TERMINAL_HANDLE_FILE);
    let closing_path = directory.join(TERMINAL_CLOSING_FILE);
    if directory.join(TERMINAL_TOMBSTONE_FILE).exists() {
        let consume_result = consume_terminal_handle(directory, None);
        let close_result = mark_session_closed(directory, None);
        consume_result?;
        return close_result;
    }
    match fs::rename(&terminal_path, &closing_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if closing_path.exists() {
                return Ok(());
            }
            return mark_session_closed(directory, None);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim terminal handle {}",
                    terminal_path.display()
                )
            });
        }
    }

    let terminal = match read_json::<terminal::TerminalSession>(&closing_path) {
        Ok(terminal) => terminal,
        Err(error) => {
            restore_terminal_handle(&closing_path, &terminal_path)?;
            return Err(error);
        }
    };
    if let Err(error) = close_terminal(&terminal) {
        restore_terminal_handle(&closing_path, &terminal_path)?;
        return Err(error);
    }

    let consume_result = consume_terminal_handle(directory, Some(terminal.kind));
    let close_result = mark_session_closed(directory, None);
    consume_result?;
    close_result
}

fn restore_terminal_handle(closing_path: &Path, terminal_path: &Path) -> Result<()> {
    fs::rename(closing_path, terminal_path).with_context(|| {
        format!(
            "failed to restore terminal handle {} after close failure",
            terminal_path.display()
        )
    })
}

fn consume_terminal_handle(
    directory: &Path,
    terminal_kind: Option<terminal::TerminalKind>,
) -> Result<()> {
    let tombstone_result = write_json_atomic(
        &directory.join(TERMINAL_TOMBSTONE_FILE),
        &serde_json::json!({
            "consumed": true,
            "terminal": terminal_kind.map(terminal::TerminalKind::as_str),
        }),
    );
    let active_result = remove_file_if_present(&directory.join(TERMINAL_HANDLE_FILE));
    let closing_result = remove_file_if_present(&directory.join(TERMINAL_CLOSING_FILE));
    tombstone_result?;
    active_result?;
    closing_result
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn emit_session_result(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    event: Option<&SessionEvent>,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "session": id,
                "provider": provider.as_str(),
                "terminal": terminal_session.kind.as_str(),
                "terminal_session_id": terminal_session.id,
                "terminal_tab_id": terminal_session.tab_id,
                "terminal_window_id": terminal_session.window_id,
                "iterm_session_id": (terminal_session.kind == terminal::TerminalKind::Iterm2)
                    .then_some(terminal_session.id.as_str()),
                "result": event.map(|value| value.message.as_str()),
                "provider_session_id": event.and_then(|value| value.provider_session_id.as_deref()),
                "turn_id": event.and_then(|value| value.turn_id.as_deref()),
            }))?
        );
    } else {
        println!("session: {id}");
        if let Some(event) = event {
            println!();
            println!("{}", terminal_safe_text(&event.message, true));
        }
    }
    Ok(())
}

fn run_session(id: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("native-session must run in a visible interactive terminal");
    }
    let directory = session_directory(id)?;
    let owner = current_native_session_owner(id)?;
    write_json_atomic(&directory.join(SESSION_OWNER_FILE), &owner)?;
    let result = run_session_inner(&directory);
    let _ = release_turn_claim(&directory);
    if let Err(error) = &result {
        let _ = update_status(&directory, "failed", None, Some(format!("{error:#}")));
    }
    result
}

fn run_session_inner(directory: &Path) -> Result<()> {
    let manifest = read_manifest(directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = directory.join("initial-prompt.txt");
    let prompt = fs::read_to_string(&prompt_path).context("failed to read initial prompt")?;
    fs::remove_file(&prompt_path).context("failed to remove consumed initial prompt")?;

    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut arguments = provider_launch_args(provider, manifest.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = &manifest.effort {
        arguments.extend(
            provider_effort_args(provider, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = &manifest.model {
        arguments.extend(
            provider_model_args(provider, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    let provider::LaunchPlan {
        arguments: provider_arguments,
        prompt_is_positional,
        completion_monitor,
    } = provider::prepare_launch(
        provider,
        provider::LaunchContext {
            bridge_executable: &executable,
            directory,
            workspace: &manifest.workspace,
            title: &manifest.title,
            prompt: &prompt,
        },
    )?;
    arguments.extend(provider_arguments);
    if prompt_is_positional {
        arguments.push(OsString::from(prompt));
    }

    update_status(directory, "running", None, None)?;
    let (agy_monitor, pi_failure_monitor) = match completion_monitor {
        provider::CompletionMonitor::Hook => (None, None),
        provider::CompletionMonitor::AgyTranscript { log_path } => (
            Some(AgyMonitor::start(directory, &log_path, &agy_brain_root()?)?),
            None,
        ),
        provider::CompletionMonitor::PiHookFailure => {
            (None, Some(PiFailureMonitor::start(directory)?))
        }
    };
    let mut provider_command =
        provider_process_command(&manifest.provider_path, directory, arguments)?;
    let status = provider_command
        .current_dir(&manifest.workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", &manifest.id)
        .env("AGENT_BRIDGE_EXECUTABLE", &executable)
        .status();
    if let Some(monitor) = agy_monitor {
        monitor.stop()?;
    }
    if let Some(monitor) = pi_failure_monitor {
        monitor.stop()?;
    }
    let status = status.with_context(|| {
        format!(
            "failed to start {} at {}",
            provider.as_str(),
            manifest.provider_path.display()
        )
    })?;
    update_status(directory, "exited", status.code(), None)?;
    if !status.success() {
        bail!("{} exited with {status}", provider.as_str());
    }
    Ok(())
}

fn run_hook(provider: FirstPartyCli, argument_payload: Option<&str>) -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os(SESSION_DIR_ENV).context("native hook session directory is not set")?,
    );
    validate_hook_directory(&directory)?;
    let manifest = read_manifest(&directory)?;
    if manifest.provider != provider.as_str() {
        bail!(
            "hook provider {} does not match session provider {}",
            provider.as_str(),
            manifest.provider
        );
    }

    let payload_text = if let Some(payload) = argument_payload {
        payload.to_owned()
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    let payload: serde_json::Value =
        serde_json::from_str(&payload_text).context("native hook received invalid JSON")?;
    let provider_session_id = json_string(&payload, &["session_id", "thread-id", "thread_id"]);
    let turn_id = json_string(&payload, &["turn_id", "turn-id"]);
    if let Some(error) = payload
        .get("agent_bridge_error")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return record_provider_failure(&directory, provider, error, provider_session_id, turn_id);
    }
    let message = extract_assistant_message(&payload)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("native hook payload did not contain an assistant result")?;
    record_provider_result(&directory, provider, message, provider_session_id, turn_id)
}

fn record_provider_result(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    write_event(directory, &event)?;
    update_status(directory, "ready", None, None)?;
    release_turn_claim(directory)
}

fn record_provider_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    write_event(directory, &event)?;
    update_status(directory, "ready", None, Some(error))?;
    release_turn_claim(directory)
}

pub(crate) fn extract_assistant_message(payload: &serde_json::Value) -> Option<&str> {
    ["last-assistant-message", "last_assistant_message"]
        .into_iter()
        .find_map(|key| payload.get(key).and_then(serde_json::Value::as_str))
}

fn json_string(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn pi_bridge_extension() -> &'static str {
    r#"import { spawnSync } from "node:child_process";
import { renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function assistantText(message) {
  if (!Array.isArray(message.content)) return undefined;
  const text = message.content
    .filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text)
    .join("\n")
    .trim();
  return text || undefined;
}

function lastAssistantOutcome(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "stop") {
      const detail = typeof message.errorMessage === "string" && message.errorMessage.trim()
        ? `: ${message.errorMessage.trim()}`
        : "";
      return { agent_bridge_error: `Pi turn ended with ${message.stopReason ?? "an unknown state"}${detail}` };
    }
    const text = assistantText(message);
    if (text) return { last_assistant_message: text };
    return { agent_bridge_error: "Pi settled without assistant text." };
  }
  return { agent_bridge_error: "Pi settled without an assistant result." };
}

function wait(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function deliverResult(executable, payload) {
  if (!executable) {
    return { ok: false, detail: "Agent Bridge executable is unavailable" };
  }
  let detail = "native hook failed";
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const result = spawnSync(executable, ["native-hook", "pi"], {
      input: JSON.stringify(payload),
      encoding: "utf8",
      stdio: ["pipe", "ignore", "pipe"],
    });
    if (!result.error && result.status === 0) return { ok: true };
    const stderr = typeof result.stderr === "string" ? result.stderr.trim() : "";
    detail = result.error?.message || stderr || `native hook exited with status ${result.status}`;
    if (attempt < 2) await wait(100 * (attempt + 1));
  }
  return { ok: false, detail: detail.slice(0, 1024) };
}

function persistHookFailure(payload, detail) {
  const directory = process.env.AGENT_BRIDGE_NATIVE_SESSION_DIR;
  if (!directory) return false;
  const target = join(directory, "pi-hook-failure.json");
  const temporary = join(
    directory,
    `.pi-hook-failure-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`,
  );
  const signal = {
    error: `Pi result delivery failed: ${detail}`,
    provider_session_id: payload.session_id,
    turn_id: payload.turn_id,
  };
  try {
    writeFileSync(temporary, JSON.stringify(signal), { encoding: "utf8", flag: "wx", mode: 0o600 });
    renameSync(temporary, target);
    return true;
  } catch {
    try { unlinkSync(temporary); } catch {}
    return false;
  }
}

export default function (pi) {
  let pending;
  let undelivered = false;

  pi.on("agent_start", (_event, ctx) => {
    if (undelivered && pending) {
      if (!persistHookFailure(pending, "a prior result remained undelivered")) {
        ctx.ui.notify("Agent Bridge still cannot recover the previous Pi result.", "warning");
        return;
      }
      undelivered = false;
    }
    pending = undefined;
  });

  pi.on("agent_end", (event, ctx) => {
    if (undelivered) return;
    pending = {
      ...lastAssistantOutcome(event.messages),
      session_id: ctx.sessionManager.getSessionId(),
      turn_id: ctx.sessionManager.getLeafId() ?? undefined,
    };
  });

  pi.on("agent_settled", async (_event, ctx) => {
    if (!pending) return;
    const payload = pending;
    const executable = process.env.AGENT_BRIDGE_EXECUTABLE;
    const delivery = await deliverResult(executable, payload);
    if (!delivery.ok) {
      if (persistHookFailure(payload, delivery.detail)) {
        pending = undefined;
        ctx.ui.notify("Agent Bridge marked this undelivered Pi result as failed.", "warning");
        return;
      }
      undelivered = true;
      ctx.ui.notify("Agent Bridge could not record or recover this Pi result.", "warning");
      return;
    }
    pending = undefined;
  });
}
"#
}

struct PiFailureMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

impl PiFailureMonitor {
    fn start(directory: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-pi-failure-monitor".to_owned())
            .spawn(move || {
                let result = monitor_pi_hook_failures(&directory, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = update_status(
                        &error_directory,
                        "failed",
                        None,
                        Some(format!("Pi result recovery monitor failed: {error:#}")),
                    );
                    let _ = release_turn_claim(&error_directory);
                }
                result
            })
            .context("failed to start Pi result recovery monitor")?;
        Ok(Self { stop, handle })
    }

    fn stop(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("Pi result recovery monitor panicked"))??;
        Ok(())
    }
}

fn monitor_pi_hook_failures(directory: &Path, stop: &AtomicBool) -> Result<()> {
    loop {
        consume_pi_hook_failure(directory)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn consume_pi_hook_failure(directory: &Path) -> Result<bool> {
    let path = directory.join(PI_HOOK_FAILURE_FILE);
    let Some(text) = read_regular_text_if_present(&path)? else {
        return Ok(false);
    };
    let signal: PiHookFailureSignal =
        serde_json::from_str(&text).context("invalid Pi hook failure recovery signal")?;
    let error = signal.error.trim();
    if error.is_empty() {
        bail!("Pi hook failure recovery signal has no error");
    }
    fs::remove_file(&path).context("failed to consume Pi hook failure recovery signal")?;
    record_provider_failure(
        directory,
        FirstPartyCli::Pi,
        error,
        signal.provider_session_id,
        signal.turn_id,
    )?;
    Ok(true)
}

struct AgyMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

impl AgyMonitor {
    fn start(directory: &Path, log_path: &Path, brain_root: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let log_path = log_path.to_owned();
        let brain_root = brain_root.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let error_directory = directory.clone();
        let handle = thread::Builder::new()
            .name("agent-bridge-agy-monitor".to_owned())
            .spawn(move || {
                let result =
                    monitor_agy_session(&directory, &log_path, &brain_root, &stop_for_thread);
                if let Err(error) = &result {
                    let _ = update_status(
                        &error_directory,
                        "failed",
                        None,
                        Some(format!("Agy result monitor failed: {error:#}")),
                    );
                    let _ = release_turn_claim(&error_directory);
                }
                result
            })
            .context("failed to start Agy result monitor")?;
        Ok(Self { stop, handle })
    }

    fn stop(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("Agy result monitor panicked"))??;
        Ok(())
    }
}

struct AgyTranscriptCursor {
    path: PathBuf,
    full_path: PathBuf,
    offset: u64,
    partial_line: Vec<u8>,
    pending_results: VecDeque<AgyPlannerResult>,
    greatest_result_step: Option<u64>,
}

impl AgyTranscriptCursor {
    fn new(path: PathBuf) -> Self {
        let full_path = path.with_file_name("transcript_full.jsonl");
        Self {
            path,
            full_path,
            offset: 0,
            partial_line: Vec::new(),
            pending_results: VecDeque::new(),
            greatest_result_step: None,
        }
    }

    fn poll(&mut self, directory: &Path, brain_root: &Path, conversation_id: &str) -> Result<()> {
        let Some(metadata) = validated_agy_file_metadata(&self.path, brain_root)? else {
            return Ok(());
        };
        if metadata.len() < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.pending_results.clear();
        }

        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.partial_line.extend_from_slice(&bytes);

        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in self.partial_line.iter().enumerate() {
            if *byte == b'\n' {
                lines.push(String::from_utf8_lossy(&self.partial_line[start..index]).into_owned());
                start = index + 1;
            }
        }
        if start > 0 {
            self.partial_line.drain(..start);
        }

        for line in lines {
            let Some(result) = parse_agy_planner_result(&line) else {
                continue;
            };
            if self
                .greatest_result_step
                .is_some_and(|previous| result.step <= previous)
                || self
                    .pending_results
                    .iter()
                    .any(|pending| pending.step == result.step)
            {
                continue;
            }
            self.pending_results.push_back(result);
        }

        while let Some(result) = self.pending_results.front() {
            let message = if result.truncated {
                let Some(message) = read_agy_full_result(&self.full_path, brain_root, result.step)?
                else {
                    break;
                };
                message
            } else {
                result.message.clone()
            };
            let step = result.step;
            record_provider_result(
                directory,
                FirstPartyCli::Agy,
                &message,
                Some(conversation_id.to_owned()),
                Some(step.to_string()),
            )?;
            self.greatest_result_step = Some(step);
            self.pending_results.pop_front();
        }
        Ok(())
    }
}

fn validated_agy_file_metadata(path: &Path, brain_root: &Path) -> Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular Agy transcript: {}", path.display());
    }
    let canonical_root = brain_root
        .canonicalize()
        .context("Agy brain directory is unavailable")?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("Agy transcript cannot be resolved: {}", path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!(
            "refusing Agy transcript outside its data directory: {}",
            path.display()
        );
    }
    Ok(Some(metadata))
}

fn read_agy_full_result(path: &Path, brain_root: &Path, step: u64) -> Result<Option<String>> {
    if validated_agy_file_metadata(path, brain_root)?.is_none() {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read full Agy transcript: {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("step_index").and_then(serde_json::Value::as_u64) != Some(step) {
            continue;
        }
        let result = parse_agy_planner_value(&value)
            .context("full Agy transcript row did not contain a final response")?;
        if result.truncated {
            bail!("full Agy transcript unexpectedly marked step {step} as truncated");
        }
        return Ok(Some(result.message));
    }
    Ok(None)
}

#[derive(Default)]
struct AgyMonitorState {
    conversation_id: Option<String>,
    transcript: Option<AgyTranscriptCursor>,
}

impl AgyMonitorState {
    fn poll(&mut self, directory: &Path, log_path: &Path, brain_root: &Path) -> Result<()> {
        if let Some(log) = read_regular_text_if_present(log_path)?
            && let Some(newest_id) = parse_agy_conversation_id(&log)
            && self.conversation_id.as_deref() != Some(newest_id.as_str())
        {
            let path = brain_root
                .join(&newest_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            self.conversation_id = Some(newest_id);
            self.transcript = Some(AgyTranscriptCursor::new(path));
        }
        if let (Some(id), Some(cursor)) =
            (self.conversation_id.as_deref(), self.transcript.as_mut())
        {
            cursor.poll(directory, brain_root, id)?;
        }
        Ok(())
    }
}

fn monitor_agy_session(
    directory: &Path,
    log_path: &Path,
    brain_root: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let mut state = AgyMonitorState::default();
    loop {
        state.poll(directory, log_path, brain_root)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn read_regular_text_if_present(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing non-regular session file: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn agy_brain_root() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    Ok(home.join(".gemini").join("antigravity-cli").join("brain"))
}

fn parse_agy_conversation_id(log: &str) -> Option<String> {
    log.lines().rev().find_map(|line| {
        let (_, suffix) = line.rsplit_once("Created conversation ")?;
        let candidate = suffix.split_whitespace().next()?;
        valid_uuid(candidate).then(|| candidate.to_owned())
    })
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Debug)]
struct AgyPlannerResult {
    step: u64,
    message: String,
    truncated: bool,
}

fn parse_agy_planner_result(line: &str) -> Option<AgyPlannerResult> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    parse_agy_planner_value(&value)
}

fn parse_agy_planner_value(value: &serde_json::Value) -> Option<AgyPlannerResult> {
    if value.get("type")?.as_str()? != "PLANNER_RESPONSE"
        || value.get("status")?.as_str()? != "DONE"
        || value.get("source")?.as_str()? != "MODEL"
    {
        return None;
    }
    if value
        .get("tool_calls")
        .is_some_and(|tool_calls| match tool_calls {
            serde_json::Value::Null => false,
            serde_json::Value::Array(calls) => !calls.is_empty(),
            _ => true,
        })
    {
        return None;
    }
    let step = value.get("step_index")?.as_u64()?;
    let message = value.get("content")?.as_str()?.trim();
    (!message.is_empty()).then(|| AgyPlannerResult {
        step,
        message: message.to_owned(),
        truncated: value
            .get("is_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
fn parse_agy_transcript_line(line: &str) -> Option<(u64, String)> {
    let result = parse_agy_planner_result(line)?;
    (!result.truncated).then_some((result.step, result.message))
}

fn create_session(spec: SessionSpec) -> Result<CreatedSession> {
    let root = state_root()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create state directory {}", root.display()))?;
    set_private_directory_permissions(&root)?;
    let temp = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&root)?;
    let directory = temp.keep();
    set_private_directory_permissions(&directory)?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("session directory name is not UTF-8")?
        .to_owned();
    require_valid_session_id(&id)?;
    let events = directory.join("events");
    fs::create_dir(&events)?;
    set_private_directory_permissions(&events)?;
    let manifest = SessionManifest {
        schema: SESSION_SCHEMA,
        id: id.clone(),
        provider: spec.provider.as_str().to_owned(),
        provider_path: spec.provider_path,
        provider_version: spec.provider_version,
        workspace: spec.workspace,
        title: spec.title,
        model: spec.model,
        effort: spec.effort,
        yolo: spec.yolo,
        created_unix_ms: unix_ms(),
    };
    write_json_atomic(&directory.join("manifest.json"), &manifest)?;
    write_private(
        &directory.join("initial-prompt.txt"),
        spec.prompt.as_bytes(),
    )?;
    update_status(&directory, "launching", None, None)?;
    Ok(CreatedSession {
        id,
        directory,
        manifest,
    })
}

fn state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(root));
    }
    default_state_root(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn default_state_root(
    home: Option<&std::ffi::OsStr>,
    user_profile: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let home = home
        .or(user_profile)
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")?;
    Ok(home.join(".agent-bridge").join("native-sessions"))
}

fn session_directory(id: &str) -> Result<PathBuf> {
    require_valid_session_id(id)?;
    let directory = state_root()?.join(id);
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("no such Agent Bridge session: {id}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("refusing non-directory Agent Bridge session: {id}");
    }
    Ok(directory)
}

fn validate_hook_directory(directory: &Path) -> Result<()> {
    let root = state_root()?
        .canonicalize()
        .context("native state root is missing")?;
    let canonical = directory
        .canonicalize()
        .context("native hook session directory is missing")?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "refusing native hook directory outside state root: {}",
            directory.display()
        );
    }
    let id = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .context("native hook session id is not UTF-8")?;
    require_valid_session_id(id)
}

fn read_manifest(directory: &Path) -> Result<SessionManifest> {
    let manifest: SessionManifest = read_json(&directory.join("manifest.json"))?;
    if manifest.schema != SESSION_SCHEMA {
        bail!(
            "unsupported session schema {} for {}",
            manifest.schema,
            manifest.id
        );
    }
    let expected_id = directory.file_name().and_then(|name| name.to_str());
    if expected_id != Some(manifest.id.as_str()) {
        bail!("session manifest id does not match its directory");
    }
    Ok(manifest)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".agent-bridge-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(&serde_json::to_vec_pretty(value)?)?;
    temporary.flush()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    set_private_file_permissions(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn update_status(
    directory: &Path,
    state: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    let status = SessionStatus {
        state: state.to_owned(),
        updated_unix_ms: unix_ms(),
        exit_code,
        error,
    };
    let status_path = directory.join("status.json");
    let closed_path = directory.join(CLOSED_STATUS_FILE);
    if state == "closed" {
        write_json_atomic(&closed_path, &status)?;
        return write_json_atomic(&status_path, &status);
    }
    if let Some(closed) = read_status_if_present(&closed_path)? {
        return write_json_atomic(&status_path, &closed);
    }
    write_json_atomic(&status_path, &status)?;
    if let Some(closed) = read_status_if_present(&closed_path)? {
        write_json_atomic(&status_path, &closed)?;
    }
    Ok(())
}

fn read_status_if_present(path: &Path) -> Result<Option<SessionStatus>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

struct TurnClaim {
    path: PathBuf,
    token: String,
    retained: bool,
}

impl TurnClaim {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if !self.retained {
            let _ = release_turn_claim_token(&self.path, &self.token);
        }
    }
}

fn acquire_turn_claim(directory: &Path) -> Result<TurnClaim> {
    let path = directory.join(TURN_CLAIM_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| "another tell request already owns this session turn")?;
    set_private_file_permissions(&file)?;
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        TURN_CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    writeln!(file, "{token}")?;
    file.flush()?;
    Ok(TurnClaim {
        path,
        token,
        retained: false,
    })
}

fn release_turn_claim_token(path: &Path, expected_token: &str) -> Result<()> {
    let token = match fs::read_to_string(path) {
        Ok(token) => token,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect native turn claim"),
    };
    if token.trim() != expected_token {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to release native turn claim"),
    }
}

fn release_turn_claim(directory: &Path) -> Result<()> {
    match fs::remove_file(directory.join(TURN_CLAIM_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to release native turn claim"),
    }
}

fn mark_session_closed(directory: &Path, error: Option<String>) -> Result<()> {
    let consume_result = if directory.join(TERMINAL_HANDLE_FILE).exists()
        || directory.join(TERMINAL_CLOSING_FILE).exists()
    {
        consume_terminal_handle(directory, None)
    } else {
        Ok(())
    };
    let status_result = update_status(directory, "closed", None, error);
    let claim_result = release_turn_claim(directory);
    consume_result?;
    status_result?;
    claim_result
}

fn repair_dead_native_owner(directory: &Path) -> Result<bool> {
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(
        status.state.as_str(),
        "launching" | "running" | "working" | "ready" | "exited" | "failed"
    ) {
        return Ok(false);
    }
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner = match fs::read_to_string(&owner_path) {
        Ok(text) => serde_json::from_str::<NativeSessionOwner>(&text)
            .with_context(|| format!("invalid JSON in {}", owner_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", owner_path.display()));
        }
    };
    #[cfg(windows)]
    if let Some(identity) = &owner.windows_process_identity
        && terminal::verify_windows_process_identity(owner.pid, identity).is_ok()
    {
        return Ok(false);
    }
    #[cfg(not(windows))]
    if process_is_alive(owner.pid) {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        let repair_error = status.error.clone().or_else(|| {
            Some(format!(
                "native session process {} is no longer running",
                owner.pid
            ))
        });
        if matches!(status.state.as_str(), "exited" | "failed") {
            mark_session_closed(directory, repair_error)?;
            return Ok(true);
        }
        // The visible console root can outlive a failed native-session owner. Reuse the same
        // atomic terminal-handle claim as explicit close so concurrent repair callers cannot
        // perform the external close side effect twice.
        close_session_state(directory, |session| {
            if session.kind != terminal::TerminalKind::WindowsConsole {
                bail!("dead Windows native owner has a non-Windows terminal handle")
            }
            terminal::close_session(session)
        })
        .context("failed to close a Windows console whose native owner exited")?;
        update_status(directory, "closed", None, repair_error)?;
        Ok(true)
    }
    #[cfg(not(windows))]
    {
        mark_session_closed(
            directory,
            status.error.or_else(|| {
                Some(format!(
                    "native session process {} is no longer running",
                    owner.pid
                ))
            }),
        )?;
        Ok(true)
    }
}

fn write_event(directory: &Path, event: &SessionEvent) -> Result<()> {
    let events = directory.join("events");
    let name = format!(
        "event-{}-{}.json",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    );
    write_json_atomic(&events.join(name), event)
}

fn event_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let events = directory.join("events");
    let mut paths = Vec::new();
    if !events.is_dir() {
        return Ok(paths);
    }
    for entry in fs::read_dir(events)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("event-") && name.ends_with(".json"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn wait_for_event(directory: &Path, baseline: usize, timeout: Duration) -> Result<SessionEvent> {
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    loop {
        repair_dead_native_owner(directory)?;
        let paths = event_paths(directory)?;
        if paths.len() > baseline {
            let event: SessionEvent = read_json(paths.last().context("event path disappeared")?)?;
            if let Some(error) = event.error.as_deref() {
                bail!("{error}");
            }
            return Ok(event);
        }
        if let Ok(status) = read_json::<SessionStatus>(&directory.join("status.json"))
            && matches!(status.state.as_str(), "failed" | "exited" | "closed")
        {
            let reason = status
                .error
                .unwrap_or_else(|| format!("session entered state {}", status.state));
            bail!("{reason}");
        }
        if Instant::now() >= deadline {
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn resolve_provider(provider: FirstPartyCli) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_provider_from_path(provider, &path)
}

fn resolve_provider_from_path(provider: FirstPartyCli, path: &std::ffi::OsStr) -> Result<PathBuf> {
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        #[cfg(windows)]
        let names = [
            format!("{}.exe", provider.command()),
            format!("{}.ps1", provider.command()),
            format!("{}.cmd", provider.command()),
            format!("{}.bat", provider.command()),
        ];
        #[cfg(not(windows))]
        let names = [provider.command().to_owned()];
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() && is_executable(&candidate) {
                let canonical = candidate.canonicalize().with_context(|| {
                    format!(
                        "failed to canonicalize provider path {}",
                        candidate.display()
                    )
                })?;
                if canonical.is_file() && is_executable(&canonical) {
                    return Ok(canonical);
                }
            }
        }
    }
    bail!(
        "{} was not found on an absolute PATH entry",
        provider.command()
    )
}

fn check_provider_version(provider: FirstPartyCli, executable: &Path) -> Result<String> {
    let mut command = provider_version_command(executable)?;
    let output = command
        .arg("--version")
        .output()
        .with_context(|| format!("failed to query {} --version", executable.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            executable.display(),
            output.status
        );
    }
    let mut version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        version = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    if !cli_version_is_supported(provider, &version)? {
        bail!(
            "{} is too old: found {version:?}, require >= {}",
            provider.as_str(),
            provider.minimum_version()
        );
    }
    Ok(version)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn shell_quote(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn bridge_shell_command(
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
    Ok(format!(
        "cd {} && {}={} {} native-session {}",
        shell_quote(workspace.as_os_str()),
        STATE_DIR_ENV,
        shell_quote(state_root.as_os_str()),
        shell_quote(executable.as_os_str()),
        shell_quote(OsString::from(id).as_os_str())
    ))
}

#[cfg(windows)]
fn powershell_quote(value: &std::ffi::OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

#[cfg(windows)]
fn bridge_shell_command(
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
    Ok(format!(
        "Set-Location -LiteralPath {}; $env:{} = {}; & {} native-session {}",
        powershell_quote(workspace.as_os_str()),
        STATE_DIR_ENV,
        powershell_quote(state_root.as_os_str()),
        powershell_quote(executable.as_os_str()),
        powershell_quote(std::ffi::OsStr::new(id)),
    ))
}

fn validate_shell_command_component(value: &std::ffi::OsStr, field: &str) -> Result<()> {
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

fn session_accepts_prompt(state: &str) -> bool {
    state == "ready"
}

fn delegation_source() -> String {
    std::env::var("AGENT_BRIDGE_NATIVE_SESSION_ID").unwrap_or_else(|_| "external".to_owned())
}

fn native_delegation_prompt(source: &str, prompt: &str) -> String {
    let source = source
        .chars()
        .take(128)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let source = if source.trim().is_empty() {
        "external"
    } else {
        source.trim()
    };
    format!(
        "[Agent Bridge native delegation]\nSource: {}\n\n{}",
        source,
        prompt.trim()
    )
}

fn terminal_paste_bytes(prompt: &str) -> Vec<u8> {
    format!("\x1b[200~{prompt}\x1b[201~").into_bytes()
}

fn sanitize_title(value: &str) -> Result<String> {
    let title = value
        .chars()
        .take(80)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        bail!("--title cannot be empty");
    }
    Ok(title)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    terminal::windows_set_private_permissions(path, true)
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
fn set_private_file_permissions(file: &fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let mut path = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path.as_mut_ptr(),
            path.len() as u32,
            0,
        )
    };
    if length == 0 || length as usize >= path.len() {
        return Err(std::io::Error::last_os_error()).context("failed to resolve private file path");
    }
    path.truncate(length as usize);
    terminal::windows_set_private_permissions(&PathBuf::from(String::from_utf16(&path)?), false)
}
