#[cfg(test)]
mod tests;

mod doctor;
mod provider;
mod provider_process;
mod query;
mod requests;
mod terminal;

use provider_process::{
    command as provider_process_command, version_command as provider_version_command,
};

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agent_bridge::{
    FirstPartyCli, checked_deadline_from, cli_version_is_supported, confirm_explicit_close,
    process_is_alive, provider_effort_args, provider_launch_args, provider_model_args,
    terminal_safe_text, validate_terminal_input,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const STATE_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_STATE_DIR";
const SESSION_DIR_ENV: &str = "AGENT_BRIDGE_NATIVE_SESSION_DIR";
const DEFAULT_TIMEOUT_SECS: u64 = 900;
const SESSION_SCHEMA: u32 = 1;
const TURN_CLAIM_FILE: &str = "turn.claim";
const TURN_CLAIM_LOCK_FILE: &str = "turn.claim.lock";
const TURN_COMPLETION_FILE: &str = "turn.completion.json";
const STATUS_LOCK_FILE: &str = "status.lock";
const SESSION_OWNER_FILE: &str = "native-session.json";
const CLOSED_STATUS_FILE: &str = "closed.json";
const TERMINAL_HANDLE_FILE: &str = "terminal.json";
const TERMINAL_CLOSING_FILE: &str = "terminal.closing.json";
const TERMINAL_TOMBSTONE_FILE: &str = "terminal.closed.json";
// v0.0.2 native-Windows Claude sessions may still carry these files. New sessions never
// create them; explicit close and prune consume them so an upgrade cannot strand state.
const LEGACY_RESUME_PENDING_FILE: &str = "resume.pending.json";
const LEGACY_RESUME_RUNNING_FILE: &str = "resume.running.json";
static TURN_CLAIM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum NativeCommand {
    Ask(AskRequest),
    Tell(TellRequest),
    Inspect {
        id: String,
        json: bool,
    },
    Result(query::ResultRequest),
    Search(query::SearchRequest),
    Doctor(doctor::DoctorRequest),
    Sessions(SessionsRequest),
    Prune(PruneRequest),
    Close(CloseRequest),
    RunSession {
        id: String,
    },
    Hook {
        provider: FirstPartyCli,
        payload: Option<String>,
    },
    ProviderControl {
        provider: FirstPartyCli,
        arguments: Vec<String>,
    },
    ConsoleControl {
        action: String,
        id: String,
        input_name: Option<String>,
        timeout_ms: Option<u64>,
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
pub(crate) struct SessionsRequest {
    pub(crate) json: bool,
    workspace: Option<PathBuf>,
    provider: Option<FirstPartyCli>,
    state: Option<String>,
    sort_updated: bool,
}

#[derive(Debug)]
pub(crate) struct CloseRequest {
    id: String,
    explicit: bool,
    json: bool,
}

#[derive(Debug)]
pub(crate) struct PruneRequest {
    closed_before_days: u64,
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
    #[serde(default)]
    generation: u64,
    updated_unix_ms: u128,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
struct PendingTurnCompletion {
    schema: u32,
    claim_token: String,
    event_file: String,
    event: SessionEvent,
    status_error: Option<String>,
    #[serde(default = "default_completion_status_state")]
    status_state: String,
}

impl PendingTurnCompletion {
    #[cfg(test)]
    fn new(claim_token: &str, event: SessionEvent, status_error: Option<String>) -> Result<Self> {
        Self::new_with_status(claim_token, event, status_error, "ready")
    }

    fn new_with_status(
        claim_token: &str,
        event: SessionEvent,
        status_error: Option<String>,
        status_state: &str,
    ) -> Result<Self> {
        if !valid_turn_claim_token(claim_token) {
            bail!("invalid native completion claim token")
        }
        if !matches!(status_state, "ready" | "failed") {
            bail!("invalid native completion status state")
        }
        Ok(Self {
            schema: 1,
            claim_token: claim_token.to_owned(),
            event_file: new_event_file_name()?,
            event,
            status_error,
            status_state: status_state.to_owned(),
        })
    }
}

fn default_completion_status_state() -> String {
    "ready".to_owned()
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
    process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_shell: Option<MacTerminalShellIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_process_identity: Option<terminal::WindowsProcessIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct MacTerminalShellIdentity {
    pid: u32,
    process_group: u32,
    terminal_tty_device: u64,
    process_start_seconds: u64,
    process_start_microseconds: u64,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeProcessIdentity {
    pid: u32,
    parent_pid: u32,
    terminal_tty_device: u64,
    process_group: u32,
    terminal_process_group: u32,
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
    parent_pid: u32,
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
    process_group: u32,
    _job_control_count: u32,
    terminal_tty_device: u32,
    terminal_process_group: u32,
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
            | "inspect"
            | "result"
            | "search"
            | "doctor"
            | "prune-sessions"
            | "close-session"
            | "native-session"
            | "native-hook"
            | "native-provider-control"
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
        "inspect" => query::parse_inspect(rest),
        "result" => query::parse_result(rest),
        "search" => query::parse_search(rest),
        "doctor" => doctor::parse_args(rest),
        "sessions" => parse_sessions(rest),
        "prune-sessions" => parse_prune(rest),
        "close-session" => parse_close(rest),
        "native-session" => {
            let id = one_positional(rest, "native-session requires one session id")?;
            require_valid_session_id(id)?;
            Ok(NativeCommand::RunSession { id: id.to_owned() })
        }
        "native-hook" => parse_hook(rest),
        "native-provider-control" => {
            let (provider, arguments) = rest
                .split_first()
                .context("native-provider-control requires a provider")?;
            let provider = FirstPartyCli::from_str(provider).map_err(anyhow::Error::msg)?;
            Ok(NativeCommand::ProviderControl {
                provider,
                arguments: arguments.to_vec(),
            })
        }
        "native-console-control" => {
            let [action, id, tail @ ..] = rest else {
                bail!("native-console-control requires an action and managed session id");
            };
            require_valid_session_id(id)?;
            if !matches!(action.as_str(), "send" | "close") {
                bail!("unsupported native console action: {action}");
            }
            let (input_name, timeout_ms) = match (action.as_str(), tail) {
                ("send", [input, timeout_ms]) if valid_pending_prompt_name(input) => {
                    let timeout_ms = timeout_ms
                        .parse::<u64>()
                        .context("invalid native console timeout")?;
                    if timeout_ms == 0 {
                        bail!("native console timeout must be positive");
                    }
                    (Some(input.clone()), Some(timeout_ms))
                }
                ("close", []) => (None, None),
                _ => bail!("invalid native console control arguments"),
            };
            Ok(NativeCommand::ConsoleControl {
                action: action.clone(),
                id: id.clone(),
                input_name,
                timeout_ms,
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
    let mut prompt_file = None;
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
            "--prompt-file" => set_once(
                &mut prompt_file,
                PathBuf::from(option_value(options, &mut index, "--prompt-file")?),
                "--prompt-file",
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
    let prompt = read_prompt_option(prompt, prompt_file, "ask")?;
    if prompt.trim().is_empty() {
        bail!("--prompt cannot be empty");
    }
    validate_terminal_input(&prompt, "--prompt")?;
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
    let mut prompt_file = None;
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
            "--prompt-file" => set_once(
                &mut prompt_file,
                PathBuf::from(option_value(options, &mut index, "--prompt-file")?),
                "--prompt-file",
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
    let prompt = read_prompt_option(prompt, prompt_file, "tell")?;
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

fn read_prompt_option(
    inline: Option<String>,
    file: Option<PathBuf>,
    command: &str,
) -> Result<String> {
    match (inline, file) {
        (Some(_), Some(_)) => bail!("{command} accepts only one of --prompt or --prompt-file"),
        (Some(prompt), None) => Ok(prompt),
        (None, Some(path)) => fs::read_to_string(&path)
            .map(|prompt| prompt.replace("\r\n", "\n"))
            .with_context(|| format!("failed to read prompt file {}", path.display())),
        (None, None) => bail!("{command} requires --prompt <text> or --prompt-file <path>"),
    }
}

fn parse_sessions(args: &[String]) -> Result<NativeCommand> {
    let mut json = false;
    let mut workspace = None;
    let mut provider = None;
    let mut state = None;
    let mut sort = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => set_flag_once(&mut json, "--json")?,
            "--workspace" => set_once(
                &mut workspace,
                PathBuf::from(option_value(args, &mut index, "--workspace")?),
                "--workspace",
            )?,
            "--provider" => set_once(
                &mut provider,
                FirstPartyCli::from_str(option_value(args, &mut index, "--provider")?)
                    .map_err(anyhow::Error::msg)?,
                "--provider",
            )?,
            "--state" => set_once(
                &mut state,
                option_value(args, &mut index, "--state")?.to_owned(),
                "--state",
            )?,
            "--sort" => {
                let value = option_value(args, &mut index, "--sort")?;
                if !matches!(value, "id" | "updated") {
                    bail!("--sort must be id or updated")
                }
                set_once(&mut sort, value.to_owned(), "--sort")?;
            }
            option => bail!("unknown sessions option: {option}"),
        }
        index += 1;
    }
    let workspace = workspace
        .map(|path| {
            path.canonicalize().or_else(|_| {
                if path.is_absolute() {
                    Ok(path)
                } else {
                    std::env::current_dir().map(|cwd| cwd.join(path))
                }
            })
        })
        .transpose()?;
    Ok(NativeCommand::Sessions(SessionsRequest {
        json,
        workspace,
        provider,
        state,
        sort_updated: sort.as_deref() == Some("updated"),
    }))
}

fn parse_prune(args: &[String]) -> Result<NativeCommand> {
    let mut closed_before_days = None;
    let mut explicit = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--closed-before-days" => {
                let value = option_value(args, &mut index, "--closed-before-days")?;
                let days = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid retention days: {value}"))?;
                if days == 0 || days.checked_mul(86_400).is_none() {
                    bail!("--closed-before-days must be a positive supported day count");
                }
                set_once(&mut closed_before_days, days, "--closed-before-days")?;
            }
            "--explicit" => set_flag_once(&mut explicit, "--explicit")?,
            "--json" => set_flag_once(&mut json, "--json")?,
            option => bail!("unknown prune-sessions option: {option}"),
        }
        index += 1;
    }
    let closed_before_days =
        closed_before_days.context("prune-sessions requires --closed-before-days N")?;
    if !explicit {
        bail!("pruning closed session records requires --explicit");
    }
    Ok(NativeCommand::Prune(PruneRequest {
        closed_before_days,
        explicit,
        json,
    }))
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
        NativeCommand::Inspect { id, json } => query::run_inspect(&id, json),
        NativeCommand::Result(request) => query::run_result(request),
        NativeCommand::Search(request) => query::run_search(request),
        NativeCommand::Doctor(request) => doctor::run(request),
        NativeCommand::Sessions(request) => run_sessions(request),
        NativeCommand::Prune(request) => run_prune(request),
        NativeCommand::Close(request) => run_close(request),
        NativeCommand::RunSession { id } => run_session(&id),
        NativeCommand::Hook { provider, payload } => run_hook(provider, payload.as_deref()),
        NativeCommand::ProviderControl {
            provider,
            arguments,
        } => provider::run_control(provider, &arguments),
        NativeCommand::ConsoleControl {
            action,
            id,
            input_name,
            timeout_ms,
        } => run_windows_console_control(&action, &id, input_name.as_deref(), timeout_ms),
    }
}

fn valid_pending_prompt_name(value: &str) -> bool {
    value.starts_with("pending-prompt-")
        && value.ends_with(".txt")
        && value.len() <= 128
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

#[cfg(target_os = "windows")]
fn run_windows_console_control(
    action: &str,
    id: &str,
    input_name: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<()> {
    let directory = session_directory(id)?;
    let manifest = read_manifest(&directory)?;
    let session: terminal::TerminalSession =
        read_json(&windows_console_handle_path(&directory, action))?;
    if session.kind != terminal::TerminalKind::WindowsConsole {
        bail!("managed session is not owned by the Windows console transport");
    }
    if action == "send" {
        verify_terminal_surface_ownership(&directory, id, &session)?;
    } else {
        session.verify_managed_session(id)?;
    }
    let input_path = input_name.map(|name| directory.join(name));
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let submit_count = provider::terminal_submit_count(provider);
    terminal::windows_console_control(
        action,
        &session,
        input_path.as_deref(),
        submit_count,
        timeout_ms.map(Duration::from_millis),
    )
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
fn run_windows_console_control(
    _action: &str,
    _id: &str,
    _input_name: Option<&str>,
    _timeout_ms: Option<u64>,
) -> Result<()> {
    bail!("native Windows console control is only available on Windows")
}

fn run_ask(request: AskRequest) -> Result<()> {
    let json = request.json;
    let mut address = None;
    let outcome = run_ask_inner(request, &mut address);
    match address {
        Some((session, request_id)) => finish_request(outcome, json, &session, &request_id),
        None => outcome,
    }
}

fn run_ask_inner(request: AskRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
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
    let provider_version =
        check_provider_version_until(request.provider, &provider_path, Some(deadline))?;
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
    let mut initial_claim = acquire_turn_claim(&created.directory)?;
    let expected_claim_token = initial_claim.token.clone();
    let receipt = initial_claim.receipt.clone();
    *address = Some((created.id.clone(), receipt.request_id.clone()));
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
    let terminal_handle_path = created.directory.join(TERMINAL_HANDLE_FILE);
    let terminal_session = match terminal::open_bound_tab(
        terminal_kind,
        &bridge_command,
        deadline,
        |session| {
            session.managed_session_id = Some(created.id.clone());
            write_json_atomic(&terminal_handle_path, session)
        },
        || remove_file_if_present(&terminal_handle_path),
    ) {
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
    let initial_prompt_transport = provider::initial_prompt_transport(request.provider);
    let mut expected_turn_id = None;
    if initial_prompt_transport == provider::InitialPromptTransport::TerminalPasteAfterLaunch {
        let mut delivery_may_have_occurred = false;
        let delivery = (|| -> Result<()> {
            wait_for_status(
                &created.directory,
                "awaiting-initial-input",
                deadline,
                request.timeout,
            )?;
            let readiness_delay = initial_prompt_delay_within_budget(
                deadline,
                provider::initial_prompt_ready_delay(request.provider),
                request.timeout,
            )?;
            thread::sleep(readiness_delay);
            let initial_prompt_path = created.directory.join("initial-prompt.txt");
            let initial_prompt = fs::read_to_string(&initial_prompt_path)
                .context("failed to read the preserved initial prompt")?;
            let initial_prompt = provider::terminal_initial_prompt(
                request.provider,
                &created.directory,
                &initial_prompt,
            )?;
            let mut prompt_file = tempfile::Builder::new()
                .prefix("pending-prompt-")
                .suffix(".txt")
                .tempfile_in(&created.directory)?;
            set_private_file_permissions(prompt_file.as_file())?;
            prompt_file.write_all(&terminal_input_bytes(
                terminal_session.kind,
                &initial_prompt,
            ))?;
            prompt_file.flush()?;
            verify_terminal_surface_ownership_until(
                &created.directory,
                &created.id,
                &terminal_session,
                deadline,
                request.timeout,
            )?;
            update_status(&created.directory, "working", None, None)?;
            let send_timeout = remaining_turn_timeout(deadline, request.timeout)?;
            provider::validate_terminal_send_budget(
                request.provider,
                terminal_session.kind,
                send_timeout,
            )?;
            match provider::send_initial_prompt(
                request.provider,
                &terminal_session,
                prompt_file.path(),
                deadline,
            ) {
                Ok(()) => delivery_may_have_occurred = true,
                Err(failure) => {
                    delivery_may_have_occurred = failure.delivery_may_have_occurred();
                    return Err(failure.into_error());
                }
            }
            initial_claim.retain_in_place();
            fs::remove_file(&initial_prompt_path)
                .context("failed to remove the delivered initial prompt")?;
            Ok(())
        })();
        if let Err(error) = delivery {
            record_initial_prompt_delivery_failure(
                &created.directory,
                &mut initial_claim,
                delivery_may_have_occurred,
                &error,
            );
            return Err(error).with_context(|| {
                format!(
                    "failed to deliver the initial prompt to {} session {}",
                    request.provider.as_str(),
                    created.id
                )
            });
        }
    } else if initial_prompt_transport
        == provider::InitialPromptTransport::ProviderCrossSessionMessageAfterLaunch
    {
        let delivery = (|| -> Result<String> {
            wait_for_status(
                &created.directory,
                "awaiting-initial-input",
                deadline,
                request.timeout,
            )?;
            let readiness_delay = initial_prompt_delay_within_budget(
                deadline,
                provider::initial_prompt_ready_delay(request.provider),
                request.timeout,
            )?;
            thread::sleep(readiness_delay);
            let request_id = provider::new_cross_session_turn_id(request.provider)?;
            let prompt_path = created.directory.join("initial-prompt.txt");
            let prompt = fs::read_to_string(&prompt_path)
                .context("failed to read the preserved initial prompt")?;
            let bridge_executable =
                std::env::current_exe().context("failed to locate agent-bridge executable")?;
            remaining_turn_timeout(deadline, request.timeout)?;
            update_status(&created.directory, "working", None, None)?;
            match provider::send_cross_session_message(
                request.provider,
                provider::CrossSessionMessageContext {
                    bridge_executable: &bridge_executable,
                    directory: &created.directory,
                    provider_path: &created.manifest.provider_path,
                    request_id: &request_id,
                    prompt: &prompt,
                    deadline,
                },
            ) {
                Ok(()) => {
                    initial_claim.retain_in_place();
                    fs::remove_file(&prompt_path)
                        .context("failed to remove the delivered initial prompt")?;
                    Ok(request_id)
                }
                Err(failure) if failure.delivery_may_have_occurred() => {
                    initial_claim.retain_in_place();
                    let error = failure.into_error();
                    let _ = update_status(
                        &created.directory,
                        "working",
                        None,
                        Some(format!("{error:#}")),
                    );
                    Err(error).context(
                        "Claude initial cross-session delivery could not be confirmed; the turn remains claimed until completion or explicit close",
                    )
                }
                Err(failure) => {
                    let error = failure.into_error();
                    let _ = update_status(
                        &created.directory,
                        "failed",
                        None,
                        Some(format!("{error:#}")),
                    );
                    Err(error).context("Claude initial cross-session delivery was not sent")
                }
            }
        })();
        match delivery {
            Ok(request_id) => expected_turn_id = Some(request_id),
            Err(error) => {
                if !initial_claim.retained {
                    let _ = update_status(
                        &created.directory,
                        "failed",
                        None,
                        Some(format!("{error:#}")),
                    );
                }
                return Err(error).with_context(|| {
                    format!(
                        "failed to deliver the initial prompt to {} session {}",
                        request.provider.as_str(),
                        created.id
                    )
                });
            }
        }
    } else {
        initial_claim.retain();
    }

    if request.detach {
        return emit_session_result(
            request.json,
            &created.id,
            &terminal_session,
            request.provider,
            &receipt.request_id,
            None,
        );
    }

    let event = wait_for_event_for_turn_until(
        &created.directory,
        0,
        expected_turn_id.as_deref(),
        Some(&expected_claim_token),
        deadline,
        request.timeout,
    )
    .with_context(|| {
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
        &receipt.request_id,
        Some(&event),
    )
}

fn verify_terminal_surface_ownership(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    verify_terminal_surface_ownership_with_timeout(directory, expected_session_id, session, None)
}

fn verify_terminal_surface_ownership_until(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    deadline: Instant,
    requested: Duration,
) -> Result<()> {
    let timeout = remaining_turn_timeout(deadline, requested)?;
    verify_terminal_surface_ownership_with_timeout(
        directory,
        expected_session_id,
        session,
        Some(timeout),
    )
}

fn verify_terminal_surface_ownership_with_timeout(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    timeout: Option<Duration>,
) -> Result<()> {
    session.verify_managed_session(expected_session_id)?;
    #[cfg(windows)]
    {
        let _ = timeout;
        verified_windows_native_owner(directory, expected_session_id)?;
    }
    #[cfg(target_os = "macos")]
    {
        let surface_tty = terminal::verify_macos_surface(session, timeout)?;
        verified_macos_terminal_owner(
            directory,
            expected_session_id,
            session,
            surface_tty.as_deref(),
        )?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (directory, timeout);
    Ok(())
}

#[cfg(windows)]
fn verified_windows_native_owner(directory: &Path, expected_session_id: &str) -> Result<()> {
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner_text = read_regular_text_if_present(&owner_path)?
        .with_context(|| "Windows terminal ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    let identity = owner
        .windows_process_identity
        .as_ref()
        .context("Windows native-session owner is missing its process identity")?;
    terminal::verify_windows_process_identity(owner.pid, identity)
        .context("Windows native-session owner identity changed")
}

#[cfg(target_os = "macos")]
fn verified_macos_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
    surface_tty: Option<&str>,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    let owner_path = directory.join(SESSION_OWNER_FILE);
    let owner_text = read_regular_text_if_present(&owner_path)?
        .with_context(|| "Terminal.app ownership requires a live native-session owner")?;
    let owner: NativeSessionOwner = serde_json::from_str(&owner_text)
        .with_context(|| format!("invalid JSON in {}", owner_path.display()))?;
    let live = live_native_process_identity(owner.pid)?;
    session.verify_managed_session(expected_session_id)?;
    if owner.managed_session_id.as_deref() != Some(expected_session_id) {
        bail!("native-session owner is not bound to this managed session")
    }
    if !matches!(
        (
            owner.terminal_tty_device,
            owner.process_start_seconds,
            owner.process_start_microseconds,
            owner.process_group,
            owner.terminal_process_group,
        ),
        (Some(_), Some(_), Some(_), Some(_), Some(_))
    ) || !native_owner_identity_matches(&owner, &live)
    {
        bail!("native-session owner birth, TTY, or foreground identity changed")
    }
    let owner_tty = owner
        .terminal_tty
        .as_deref()
        .context("native-session owner is missing its controlling TTY")?;
    if surface_tty.is_some_and(|tty| tty != owner_tty) {
        bail!("terminal surface is attached to a different native-session TTY")
    }
    if terminal_tty_device(Path::new(owner_tty))? != live.terminal_tty_device {
        bail!("native-session owner TTY path no longer identifies its controlling TTY")
    }
    verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = live_native_process_identity(live.parent_pid)?;
    verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    Ok((owner, live))
}

#[cfg(target_os = "macos")]
fn verified_apple_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<(NativeSessionOwner, NativeProcessIdentity)> {
    if session.kind != terminal::TerminalKind::AppleTerminal {
        bail!("Terminal.app ownership proof received a different terminal kind")
    }
    let surface_tty = terminal::verify_macos_surface(session, None)?
        .context("Terminal.app ownership proof did not return a TTY")?;
    verified_macos_terminal_owner(directory, expected_session_id, session, Some(&surface_tty))
}

#[cfg(target_os = "macos")]
fn terminate_apple_terminal_owner(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<()> {
    let (owner, live) = verified_apple_terminal_owner(directory, expected_session_id, session)?;
    let process_group = verified_terminal_owner_process_group(&owner, &live)?;
    let live_shell = live_native_process_identity(live.parent_pid)?;
    let shell_process_group = verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    terminal::macos::apple_terminal::terminate_process_groups(process_group, shell_process_group)
}

#[cfg(test)]
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
    let live_shell = live_native_process_identity(live.parent_pid)?;
    let terminal_shell = MacTerminalShellIdentity {
        pid: live_shell.pid,
        process_group: live_shell.process_group,
        terminal_tty_device: live_shell.terminal_tty_device,
        process_start_seconds: live_shell.process_start_seconds,
        process_start_microseconds: live_shell.process_start_microseconds,
    };
    let owner = NativeSessionOwner {
        pid,
        managed_session_id: Some(session_id.to_owned()),
        terminal_tty: Some(terminal_tty),
        terminal_tty_device: Some(terminal_tty_device),
        process_start_seconds: Some(live.process_start_seconds),
        process_start_microseconds: Some(live.process_start_microseconds),
        process_group: Some(live.process_group),
        terminal_process_group: Some(live.terminal_process_group),
        terminal_shell: Some(terminal_shell),
        windows_process_identity: None,
    };
    verified_terminal_owner_process_group(&owner, &live)?;
    verified_terminal_shell_process_group(&owner, &live, &live_shell)?;
    Ok(owner)
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
        parent_pid: info.parent_pid,
        terminal_tty_device: u64::from(info.terminal_tty_device),
        process_group: info.process_group,
        terminal_process_group: info.terminal_process_group,
        process_start_seconds: info.process_start_seconds,
        process_start_microseconds: info.process_start_microseconds,
    })
}

#[cfg(any(target_os = "macos", test))]
fn native_owner_identity_matches(owner: &NativeSessionOwner, live: &NativeProcessIdentity) -> bool {
    if owner.pid != live.pid {
        return false;
    }
    match (
        owner.terminal_tty_device,
        owner.process_start_seconds,
        owner.process_start_microseconds,
        owner.process_group,
        owner.terminal_process_group,
    ) {
        (None, None, None, None, None) => true,
        (
            Some(terminal_tty_device),
            Some(process_start_seconds),
            Some(process_start_microseconds),
            Some(process_group),
            Some(terminal_process_group),
        ) => {
            terminal_tty_device == live.terminal_tty_device
                && process_start_seconds == live.process_start_seconds
                && process_start_microseconds == live.process_start_microseconds
                && process_group == live.process_group
                && terminal_process_group == live.terminal_process_group
        }
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn mac_native_owner_is_live(owner: &NativeSessionOwner) -> Result<bool> {
    if matches!(
        (
            owner.terminal_tty_device,
            owner.process_start_seconds,
            owner.process_start_microseconds,
            owner.process_group,
            owner.terminal_process_group,
        ),
        (None, None, None, None, None)
    ) {
        // Pre-0.0.3 owner records had only a PID. Preserve their historical liveness
        // behavior; every newly launched macOS session records the strong identity below.
        return Ok(process_is_alive(owner.pid));
    }
    match live_native_process_identity(owner.pid) {
        Ok(live) => Ok(native_owner_identity_matches(owner, &live)),
        Err(error) if process_is_alive(owner.pid) => Err(error).with_context(|| {
            format!(
                "failed to verify the birth identity of live native-session process {}",
                owner.pid
            )
        }),
        Err(_) => Ok(false),
    }
}

#[cfg(any(target_os = "macos", test))]
fn verified_terminal_owner_process_group(
    owner: &NativeSessionOwner,
    live: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live.pid {
        bail!("native-session process group no longer belongs to the recorded owner")
    }
    match (owner.process_group, owner.terminal_process_group) {
        (Some(process_group), Some(terminal_process_group)) => {
            if process_group != live.process_group
                || terminal_process_group != live.terminal_process_group
            {
                bail!("native-session process group identity changed")
            }
        }
        (None, None) => {}
        _ => bail!("native-session owner has an incomplete process group identity"),
    }
    let process_group = live.process_group;
    let terminal_process_group = live.terminal_process_group;
    if process_group != owner.pid || terminal_process_group != process_group {
        bail!("native-session owner does not lead the terminal foreground process group")
    }
    Ok(process_group)
}

#[cfg(any(target_os = "macos", test))]
fn verified_terminal_shell_process_group(
    owner: &NativeSessionOwner,
    live_owner: &NativeProcessIdentity,
    live_shell: &NativeProcessIdentity,
) -> Result<u32> {
    if owner.pid != live_owner.pid || live_owner.parent_pid != live_shell.pid {
        bail!("Terminal.app shell no longer owns the native-session process")
    }
    if live_shell.terminal_tty_device != live_owner.terminal_tty_device {
        bail!("Terminal.app shell is attached to a different TTY")
    }
    if live_shell.process_group != live_shell.pid
        || live_shell.terminal_process_group != live_owner.process_group
        || live_shell.process_group == live_owner.process_group
    {
        bail!("Terminal.app shell does not own the expected foreground job")
    }
    if let Some(recorded) = &owner.terminal_shell
        && (recorded.pid != live_shell.pid
            || recorded.process_group != live_shell.process_group
            || recorded.terminal_tty_device != live_shell.terminal_tty_device
            || recorded.process_start_seconds != live_shell.process_start_seconds
            || recorded.process_start_microseconds != live_shell.process_start_microseconds)
    {
        bail!("Terminal.app shell identity changed")
    }
    Ok(live_shell.process_group)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrossSessionFailureAction {
    TerminalFallback,
    RetainClaim,
    ReturnError,
}

fn cross_session_failure_action(
    transport: provider::FollowUpTransport,
    failure: &provider::CrossSessionMessageFailure,
) -> CrossSessionFailureAction {
    if transport
        == provider::FollowUpTransport::ProviderCrossSessionMessageWithTerminalPasteFallback
        && failure.allows_terminal_fallback()
    {
        CrossSessionFailureAction::TerminalFallback
    } else if failure.delivery_may_have_occurred() {
        CrossSessionFailureAction::RetainClaim
    } else {
        CrossSessionFailureAction::ReturnError
    }
}

fn run_tell(request: TellRequest) -> Result<()> {
    let json = request.json;
    let mut address = None;
    let outcome = run_tell_inner(request, &mut address);
    match address {
        Some((session, request_id)) => finish_request(outcome, json, &session, &request_id),
        None => outcome,
    }
}

fn run_tell_inner(request: TellRequest, address: &mut Option<(String, String)>) -> Result<()> {
    let deadline = checked_deadline_from(Instant::now(), request.timeout)?;
    let directory = session_directory(&request.id)?;
    recover_pending_completion(&directory)?;
    repair_dead_native_owner(&directory)?;
    let manifest = read_manifest(&directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    let terminal_session: terminal::TerminalSession = read_json(&directory.join("terminal.json"))?;
    verify_terminal_surface_ownership_until(
        &directory,
        &request.id,
        &terminal_session,
        deadline,
        request.timeout,
    )?;
    let previous_state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&previous_state) {
        bail!(
            "session {} is {previous_state}; tell requires the ready state",
            request.id
        );
    }
    let prompt = native_delegation_prompt(&delegation_source(), &request.prompt);
    let follow_up_transport = provider::follow_up_transport(provider);
    let (mut claim, baseline) = acquire_ready_turn_claim(&directory, &request.id)?;
    let claim_token = claim.token.clone();
    let receipt = claim.receipt.clone();
    *address = Some((request.id.clone(), receipt.request_id.clone()));
    let mut expected_turn_id = None;
    match follow_up_transport {
        provider::FollowUpTransport::TerminalPasteFallback => {
            deliver_terminal_follow_up(
                provider,
                follow_up_transport,
                &directory,
                &request.id,
                &terminal_session,
                &prompt,
                &claim_token,
                &mut claim,
                deadline,
                request.timeout,
            )?;
        }
        provider::FollowUpTransport::ProviderCrossSessionMessage
        | provider::FollowUpTransport::ProviderCrossSessionMessageWithTerminalPasteFallback => {
            remaining_turn_timeout(deadline, request.timeout)?;
            let provider_turn_id = if follow_up_transport
                == provider::FollowUpTransport::ProviderCrossSessionMessage
            {
                Some(provider::new_cross_session_turn_id(provider)?)
            } else {
                None
            };
            let correlation_id = provider_turn_id.as_deref().unwrap_or(&claim_token);
            let bridge_executable =
                std::env::current_exe().context("failed to locate agent-bridge executable")?;
            update_status(&directory, "working", None, None)?;
            match provider::send_cross_session_message(
                provider,
                provider::CrossSessionMessageContext {
                    bridge_executable: &bridge_executable,
                    directory: &directory,
                    provider_path: &manifest.provider_path,
                    request_id: correlation_id,
                    prompt: &prompt,
                    deadline,
                },
            ) {
                Ok(()) => expected_turn_id = provider_turn_id,
                Err(failure)
                    if cross_session_failure_action(follow_up_transport, &failure)
                        == CrossSessionFailureAction::TerminalFallback =>
                {
                    let unavailable = failure.into_error();
                    deliver_terminal_follow_up(
                        provider,
                        follow_up_transport,
                        &directory,
                        &request.id,
                        &terminal_session,
                        &prompt,
                        &claim_token,
                        &mut claim,
                        deadline,
                        request.timeout,
                    )
                    .with_context(|| {
                        format!(
                            "provider native follow-up was unavailable ({unavailable:#}); terminal fallback also failed"
                        )
                    })?;
                }
                Err(failure)
                    if cross_session_failure_action(follow_up_transport, &failure)
                        == CrossSessionFailureAction::RetainClaim =>
                {
                    let error = failure.into_error();
                    record_follow_up_cross_session_delivery_uncertainty(
                        &directory, &mut claim, &error,
                    );
                    return Err(error).with_context(|| {
                        format!(
                            "provider follow-up transport {} could not confirm delivery; the turn remains claimed until the target reports completion or the session is explicitly closed",
                            follow_up_transport.as_str()
                        )
                    });
                }
                Err(failure) => {
                    let error = failure.into_error();
                    let _ = update_status(
                        &directory,
                        &previous_state,
                        None,
                        Some(format!("{error:#}")),
                    );
                    return Err(error).with_context(|| {
                        format!(
                            "provider follow-up transport {} failed",
                            follow_up_transport.as_str()
                        )
                    });
                }
            }
        }
    }
    claim.retain();

    if request.detach {
        return emit_session_result(
            request.json,
            &request.id,
            &terminal_session,
            provider,
            &receipt.request_id,
            None,
        );
    }
    let event = wait_for_event_for_turn_until(
        &directory,
        baseline,
        expected_turn_id.as_deref(),
        Some(&claim_token),
        deadline,
        request.timeout,
    )
    .with_context(|| {
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
        provider,
        &receipt.request_id,
        Some(&event),
    )
}

#[allow(clippy::too_many_arguments)]
fn deliver_terminal_follow_up(
    provider: FirstPartyCli,
    follow_up_transport: provider::FollowUpTransport,
    directory: &Path,
    session_id: &str,
    terminal_session: &terminal::TerminalSession,
    prompt: &str,
    claim_token: &str,
    claim: &mut TurnClaim,
    deadline: Instant,
    requested_timeout: Duration,
) -> Result<()> {
    let prepared_prompt = (|| -> Result<tempfile::NamedTempFile> {
        let correlated_prompt =
            provider::prepare_terminal_follow_up(provider, directory, prompt, claim_token)?;
        let mut prompt_file = tempfile::Builder::new()
            .prefix("pending-prompt-")
            .suffix(".txt")
            .tempfile_in(directory)?;
        set_private_file_permissions(prompt_file.as_file())?;
        prompt_file.write_all(&terminal_input_bytes(
            terminal_session.kind,
            &correlated_prompt,
        ))?;
        prompt_file.flush()?;
        verify_terminal_surface_ownership_until(
            directory,
            session_id,
            terminal_session,
            deadline,
            requested_timeout,
        )?;
        Ok(prompt_file)
    })();
    let prompt_file = match prepared_prompt {
        Ok(prompt_file) => prompt_file,
        Err(error) => {
            let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
            return Err(error)
                .with_context(|| {
                    format!(
                        "failed to prepare input for visible {} session {session_id}",
                        terminal_session.kind.display_name()
                    )
                })
                .with_context(|| {
                    format!(
                        "provider follow-up transport {} failed",
                        follow_up_transport.as_str()
                    )
                });
        }
    };
    update_status(directory, "working", None, None)?;
    let send_timeout = match remaining_turn_timeout(deadline, requested_timeout) {
        Ok(timeout) => timeout,
        Err(error) => {
            let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
            return Err(error).with_context(|| {
                format!(
                    "provider follow-up transport {} exhausted its total timeout before delivery",
                    follow_up_transport.as_str()
                )
            });
        }
    };
    if let Err(error) =
        provider::validate_terminal_send_budget(provider, terminal_session.kind, send_timeout)
    {
        let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
        return Err(error).with_context(|| {
            format!(
                "provider follow-up transport {} cannot start inside its remaining total timeout",
                follow_up_transport.as_str()
            )
        });
    }
    if let Err(failure) =
        provider::send_terminal_follow_up(provider, terminal_session, prompt_file.path(), deadline)
    {
        if !failure.delivery_may_have_occurred() {
            let _ = provider::cancel_terminal_follow_up(provider, directory, claim_token);
        }
        record_follow_up_terminal_delivery_failure(directory, claim, &failure);
        let error = failure.into_error();
        return Err(error)
            .with_context(|| {
                format!(
                    "failed to type into visible {} session {session_id}",
                    terminal_session.kind.display_name()
                )
            })
            .with_context(|| {
                format!(
                    "provider follow-up transport {} failed",
                    follow_up_transport.as_str()
                )
            });
    }
    Ok(())
}

fn remaining_turn_timeout(deadline: Instant, requested: Duration) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .with_context(|| format!("timed out after {} seconds", requested.as_secs()))
}

#[derive(Debug)]
struct CommandOutputFailure {
    error: anyhow::Error,
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", test)),
        allow(dead_code)
    )]
    process_started: bool,
}

impl CommandOutputFailure {
    fn not_started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: false,
        }
    }

    fn started(error: anyhow::Error) -> Self {
        Self {
            error,
            process_started: true,
        }
    }

    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", test)),
        allow(dead_code)
    )]
    fn process_started(&self) -> bool {
        self.process_started
    }

    fn into_error(self) -> anyhow::Error {
        self.error
    }
}

fn command_output_until(command: &mut Command, deadline: Instant, label: &str) -> Result<Output> {
    command_output_until_classified(command, deadline, label)
        .map_err(CommandOutputFailure::into_error)
}

fn command_output_until_classified(
    command: &mut Command,
    deadline: Instant,
    label: &str,
) -> std::result::Result<Output, CommandOutputFailure> {
    if Instant::now() >= deadline {
        return Err(CommandOutputFailure::not_started(anyhow::anyhow!(
            "{label} timed out before it started"
        )));
    }
    let mut stdout = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let mut stderr = tempfile::tempfile()
        .with_context(|| format!("failed to create bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let child_stdout = stdout
        .try_clone()
        .with_context(|| format!("failed to clone bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    let child_stderr = stderr
        .try_clone()
        .with_context(|| format!("failed to clone bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::not_started)?;
    if Instant::now() >= deadline {
        return Err(CommandOutputFailure::not_started(anyhow::anyhow!(
            "{label} timed out before it started"
        )));
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr))
        .spawn()
        .with_context(|| format!("failed to start {label}"))
        .map_err(CommandOutputFailure::not_started)?;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(20)));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandOutputFailure::started(anyhow::anyhow!(
                    "{label} timed out"
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandOutputFailure::started(
                    anyhow::Error::new(error).context(format!("failed to wait for {label}")),
                ));
            }
        }
    };
    stdout
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    stderr
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .with_context(|| format!("failed to read bounded stdout storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    stderr
        .read_to_end(&mut stderr_bytes)
        .with_context(|| format!("failed to read bounded stderr storage for {label}"))
        .map_err(CommandOutputFailure::started)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

fn initial_prompt_delay_within_budget(
    deadline: Instant,
    delay: Duration,
    requested: Duration,
) -> Result<Duration> {
    let remaining = remaining_turn_timeout(deadline, requested)?;
    if delay >= remaining {
        bail!(
            "initial prompt readiness delay of {} seconds does not fit inside the {} second ask timeout",
            delay.as_secs(),
            requested.as_secs()
        )
    }
    Ok(delay)
}

fn run_sessions(request: SessionsRequest) -> Result<()> {
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
            let Ok(manifest) = read_manifest(&directory) else {
                continue;
            };
            if request
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace != &manifest.workspace)
                || request
                    .provider
                    .is_some_and(|provider| provider.as_str() != manifest.provider)
            {
                continue;
            }
            let _ = recover_pending_completion(&directory);
            let _ = repair_dead_native_owner(&directory);
            let status = read_json::<SessionStatus>(&directory.join("status.json")).ok();
            let state = status
                .as_ref()
                .map(|value| value.state.as_str())
                .unwrap_or("unknown");
            if request.state.as_ref().is_some_and(|filter| filter != state) {
                continue;
            }
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
                "created_unix_ms": manifest.created_unix_ms,
                "updated_unix_ms": status.as_ref().map(|value| value.updated_unix_ms),
                "error": status.as_ref().and_then(|value| value.error.as_deref()),
                "model": manifest.model,
                "effort": manifest.effort,
                "results": event_paths(&directory).map(|paths| paths.len()).unwrap_or(0),
            }));
        }
    }
    sessions.sort_by(|left, right| {
        let by_id = left["id"].as_str().cmp(&right["id"].as_str());
        if request.sort_updated {
            right["updated_unix_ms"]
                .as_u64()
                .cmp(&left["updated_unix_ms"].as_u64())
                .then(by_id)
        } else {
            by_id
        }
    });
    if request.json {
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

fn run_prune(request: PruneRequest) -> Result<()> {
    if !request.explicit {
        bail!("pruning closed session records requires --explicit");
    }
    let retention_ms = u128::from(request.closed_before_days)
        .checked_mul(86_400_000)
        .context("closed-session retention window is too large")?;
    let now_unix_ms = unix_ms();
    let removed = if retention_ms > now_unix_ms {
        Vec::new()
    } else {
        prune_closed_sessions(&state_root()?, now_unix_ms - retention_ms)?
    };
    if request.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "closed_before_days": request.closed_before_days,
                "pruned": removed,
            }))?
        );
    } else if removed.is_empty() {
        println!("no eligible closed Agent Bridge sessions");
    } else {
        for id in &removed {
            println!("pruned {id}");
        }
        println!("pruned {} closed session(s)", removed.len());
    }
    Ok(())
}

fn prune_closed_sessions(root: &Path, cutoff_unix_ms: u128) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to resolve state directory {}", root.display()))?;
    let mut removed = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_session_id(&id) {
            continue;
        }
        let directory = entry.path();
        if !is_regular_file(&directory.join("manifest.json"))? {
            continue;
        }
        let Ok(manifest) = read_manifest(&directory) else {
            continue;
        };
        if manifest.id != id {
            continue;
        }
        let closed = match read_regular_status_if_present(&directory.join(CLOSED_STATUS_FILE)) {
            Ok(Some(closed)) => closed,
            Ok(None) | Err(_) => continue,
        };
        let status = match read_regular_status_if_present(&directory.join("status.json")) {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => continue,
        };
        if closed.state != "closed"
            || status.state != "closed"
            || closed.updated_unix_ms > cutoff_unix_ms
            || status.updated_unix_ms > cutoff_unix_ms
            || has_active_session_capability(&directory)
            || native_owner_blocks_prune(&directory)?
        {
            continue;
        }
        let canonical_directory = directory
            .canonicalize()
            .with_context(|| format!("failed to resolve session directory {id}"))?;
        if canonical_directory.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("failed to recheck session directory {id}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        fs::remove_dir_all(&directory)
            .with_context(|| format!("failed to prune closed session {id}"))?;
        removed.push(id);
    }
    removed.sort();
    Ok(removed)
}

fn has_active_session_capability(directory: &Path) -> bool {
    [
        TERMINAL_HANDLE_FILE,
        TERMINAL_CLOSING_FILE,
        TURN_CLAIM_FILE,
        TURN_COMPLETION_FILE,
        LEGACY_RESUME_PENDING_FILE,
        LEGACY_RESUME_RUNNING_FILE,
    ]
    .into_iter()
    .any(|name| fs::symlink_metadata(directory.join(name)).is_ok())
}

fn is_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn read_regular_status_if_present(path: &Path) -> Result<Option<SessionStatus>> {
    let Some(text) = read_regular_text_if_present(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn native_owner_blocks_prune(directory: &Path) -> Result<bool> {
    let path = directory.join(SESSION_OWNER_FILE);
    let text = match read_regular_text_if_present(&path) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(false),
        Err(_) => return Ok(true),
    };
    let owner = match serde_json::from_str::<NativeSessionOwner>(&text) {
        Ok(owner) => owner,
        Err(_) => return Ok(true),
    };

    #[cfg(windows)]
    {
        let Some(identity) = &owner.windows_process_identity else {
            return Ok(true);
        };
        if !process_is_alive(owner.pid) {
            return Ok(false);
        }
        match terminal::windows_process_identity(owner.pid) {
            Ok(live) => Ok(&live == identity),
            Err(_) => Ok(true),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if !process_is_alive(owner.pid) {
            return Ok(false);
        }
        let (Some(seconds), Some(microseconds)) = (
            owner.process_start_seconds,
            owner.process_start_microseconds,
        ) else {
            return Ok(true);
        };
        match live_native_process_identity(owner.pid) {
            Ok(live) => Ok(live.process_start_seconds == seconds
                && live.process_start_microseconds == microseconds),
            Err(_) => Ok(true),
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Ok(process_is_alive(owner.pid))
    }
}

fn run_close(request: CloseRequest) -> Result<()> {
    confirm_explicit_close(request.explicit)?;
    let directory = session_directory(&request.id)?;
    close_repaired_session_state(&directory, |session| {
        let has_native_owner = verify_terminal_close_authority(&directory, &request.id, session)?;
        #[cfg(target_os = "macos")]
        if has_native_owner && session.kind == terminal::TerminalKind::AppleTerminal {
            terminate_apple_terminal_owner(&directory, &request.id, session)?;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = has_native_owner;
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

fn verify_terminal_close_authority(
    directory: &Path,
    expected_session_id: &str,
    session: &terminal::TerminalSession,
) -> Result<bool> {
    #[cfg(any(target_os = "macos", windows))]
    {
        if read_regular_text_if_present(&directory.join(SESSION_OWNER_FILE))?.is_some() {
            verify_terminal_surface_ownership(directory, expected_session_id, session)?;
            return Ok(true);
        }
        let status: SessionStatus = read_json(&directory.join("status.json"))?;
        if !matches!(status.state.as_str(), "launching" | "failed") {
            bail!(
                "terminal close requires a live native-session owner while the session is {}",
                status.state
            );
        }
        // Startup can fail after the exact surface handle is durably bound but before the
        // provider wrapper writes native-session.json. Explicit close may recover only that
        // bound launch surface; the adapter still targets its stable native identifiers.
        session.verify_managed_session(expected_session_id)?;
        Ok(false)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        verify_terminal_surface_ownership(directory, expected_session_id, session)?;
        Ok(true)
    }
}

fn close_repaired_session_state<F>(directory: &Path, close_terminal: F) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let repair_error = repair_dead_native_owner(directory)
        .err()
        .map(|error| format!("pre-close session repair failed: {error:#}"));
    close_session_state_with_error(directory, repair_error, close_terminal)
}

#[cfg(test)]
fn close_session_state<F>(directory: &Path, close_terminal: F) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    close_session_state_with_error(directory, None, close_terminal)
}

fn close_session_state_with_error<F>(
    directory: &Path,
    close_error: Option<String>,
    mut close_terminal: F,
) -> Result<()>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _turn_lock = lock_turn_claim(&claim_path)?;
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if status.state == "closed" {
        let consume_result = consume_terminal_handle(directory, None);
        let close_result = mark_session_closed_locked(directory, &claim_path, close_error);
        consume_result?;
        return close_result;
    }

    let terminal_path = directory.join(TERMINAL_HANDLE_FILE);
    let closing_path = directory.join(TERMINAL_CLOSING_FILE);
    if directory.join(TERMINAL_TOMBSTONE_FILE).exists() {
        let consume_result = consume_terminal_handle(directory, None);
        let close_result = mark_session_closed_locked(directory, &claim_path, close_error);
        consume_result?;
        return close_result;
    }
    match fs::rename(&terminal_path, &closing_path) {
        Ok(()) => sync_parent_directory(&closing_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !closing_path.exists() {
                return mark_session_closed_locked(directory, &claim_path, close_error);
            }
            // A prior closer may have stopped after atomically claiming the handle but before
            // invoking the terminal adapter. The turn-claim lock serializes recovery, so resume
            // that durable close transaction instead of reporting success with a live surface.
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
    let close_result = mark_session_closed_locked(directory, &claim_path, close_error);
    consume_result?;
    close_result
}

fn restore_terminal_handle(closing_path: &Path, terminal_path: &Path) -> Result<()> {
    rename_session_file(closing_path, terminal_path).with_context(|| {
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
        Ok(()) => sync_parent_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub(super) fn rename_session_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
    sync_parent_directory(to)?;
    if from.parent() != to.parent() {
        sync_parent_directory(from)?;
    }
    Ok(())
}

fn finish_request(outcome: Result<()>, json: bool, session: &str, request_id: &str) -> Result<()> {
    if let Err(error) = outcome {
        if json {
            let mut value = session_directory(session)
                .and_then(|directory| query::request_result(&directory, request_id))
                .unwrap_or_else(|_| {
                    serde_json::json!({
                        "schema_version": 1, "session": session, "request_id": request_id,
                        "request_state": "unknown", "result": null
                    })
                });
            value["ok"] = serde_json::json!(false);
            value["error"] = serde_json::json!(format!("{error:#}"));
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        return Err(error).with_context(|| format!(
            "session {session}, request {request_id}; inspect with `agent-bridge result {session} --request {request_id} --json`; this error alone is not proof of non-delivery; inspect the recorded outcome before deciding whether to retry"
        ));
    }
    Ok(())
}

fn emit_session_result(
    json: bool,
    id: &str,
    terminal_session: &terminal::TerminalSession,
    provider: FirstPartyCli,
    request_id: &str,
    event: Option<&SessionEvent>,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "schema_version": 1,
                "session": id,
                "request_id": request_id,
                "request_state": if event.is_some() { "completed" } else { "accepted" },
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
        println!("session: {id}\nrequest: {request_id}");
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
    if let Err(finalization_error) = finalize_native_session(&directory, &result) {
        return match result {
            Ok(()) => Err(finalization_error),
            Err(run_error) => Err(anyhow::anyhow!(
                "{run_error:#}; native session finalization also failed: {finalization_error:#}"
            )),
        };
    }
    result
}

fn finalize_native_session(directory: &Path, result: &Result<()>) -> Result<()> {
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _claim_lock = lock_turn_claim(&claim_path)?;
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(status.state.as_str(), "closed" | "exited" | "failed") {
        match result {
            Ok(()) => update_status(directory, "exited", Some(0), None)?,
            Err(error) => {
                update_status(directory, "failed", None, Some(format!("{error:#}")))?;
            }
        }
    }
    remove_turn_claim_locked(&claim_path)
}

fn run_session_inner(directory: &Path) -> Result<()> {
    let manifest = read_manifest(directory)?;
    let provider = FirstPartyCli::from_str(&manifest.provider).map_err(anyhow::Error::msg)?;
    check_provider_version(provider, &manifest.provider_path)?;
    let prompt_path = directory.join("initial-prompt.txt");
    let prompt = fs::read_to_string(&prompt_path).context("failed to read initial prompt")?;
    let initial_prompt_transport = provider::initial_prompt_transport(provider);

    let executable = std::env::current_exe().context("failed to locate agent-bridge executable")?;
    let mut policy_arguments = provider_launch_args(provider, manifest.yolo)
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    if let Some(effort) = &manifest.effort {
        policy_arguments.extend(
            provider_effort_args(provider, effort)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Some(model) = &manifest.model {
        policy_arguments.extend(
            provider_model_args(provider, model)
                .into_iter()
                .map(OsString::from),
        );
    }
    let provider::LaunchPlan {
        arguments: provider_arguments,
        prompt_is_positional,
        completion_monitor,
        environment_removals,
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
    let mut arguments = policy_arguments.clone();
    arguments.extend(provider_arguments);
    if initial_prompt_transport == provider::InitialPromptTransport::ProviderArgument
        && prompt_is_positional
    {
        arguments.push(OsString::from(prompt.clone()));
    }

    let completion_monitor = completion_monitor.start(directory)?;
    let mut provider_command =
        provider_process_command(&manifest.provider_path, directory, arguments)?;
    provider::apply_environment_removals(&mut provider_command, environment_removals);
    let child = provider_command
        .current_dir(&manifest.workspace)
        .env(SESSION_DIR_ENV, directory)
        .env("AGENT_BRIDGE_NATIVE_SESSION_ID", &manifest.id)
        .env("AGENT_BRIDGE_EXECUTABLE", &executable)
        .spawn();
    let mut child = child.with_context(|| {
        format!(
            "failed to start {} at {}",
            provider.as_str(),
            manifest.provider_path.display()
        )
    })?;
    match initial_prompt_transport {
        provider::InitialPromptTransport::ProviderArgument => {
            fs::remove_file(&prompt_path)
                .context("failed to remove the accepted initial prompt")?;
            update_status(directory, "running", None, None)?;
        }
        provider::InitialPromptTransport::ProviderCrossSessionMessageAfterLaunch
        | provider::InitialPromptTransport::TerminalPasteAfterLaunch => {
            update_status(directory, "awaiting-initial-input", None, None)?;
        }
    }
    let status = child.wait();
    completion_monitor.stop()?;
    let status = status.with_context(|| {
        format!(
            "failed to start {} at {}",
            provider.as_str(),
            manifest.provider_path.display()
        )
    })?;
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
    provider::handle_hook(provider, &directory, &payload)
}

#[cfg(test)]
fn record_provider_result(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_result_for_claim(
        directory,
        provider,
        message,
        provider_session_id,
        turn_id,
        None,
    )
}

fn record_provider_result_for_claim(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
) -> Result<()> {
    record_provider_result_for_claim_condition(
        directory,
        provider,
        message,
        provider_session_id,
        turn_id,
        expected_claim_token,
        false,
    )
}

fn record_initial_provider_result(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_result_for_claim_condition(
        directory,
        provider,
        message,
        provider_session_id,
        turn_id,
        None,
        true,
    )
}

fn record_provider_result_for_claim_condition(
    directory: &Path,
    provider: FirstPartyCli,
    message: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
    require_no_prior_provider_event: bool,
) -> Result<()> {
    let expected_claim_token = match expected_claim_token {
        Some(token) => token.to_owned(),
        None => match current_turn_claim_token(directory)? {
            Some(token) => token,
            None => return Ok(()),
        },
    };
    let expected_claim_token = Some(expected_claim_token.as_str());
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _claim_lock = lock_turn_claim(&claim_path)?;
    recover_pending_completion_locked(directory, &claim_path)?;
    if require_no_prior_provider_event && provider_has_completed_turn(directory, provider)? {
        return Ok(());
    }
    if !provider_completion_is_current(
        directory,
        provider,
        provider_session_id.as_deref(),
        turn_id.as_deref(),
        expected_claim_token,
    )? {
        return Ok(());
    }
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: message.to_owned(),
        error: None,
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    commit_provider_completion_locked(
        directory,
        &claim_path,
        expected_claim_token.context("provider completion has no claim token")?,
        event,
        None,
    )
}

#[cfg(test)]
fn record_provider_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_failure_for_claim(
        directory,
        provider,
        error,
        provider_session_id,
        turn_id,
        None,
    )
}

fn record_provider_failure_for_claim(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
) -> Result<()> {
    record_provider_failure_for_claim_condition(
        directory,
        provider,
        error,
        provider_session_id,
        turn_id,
        expected_claim_token,
        false,
    )
}

fn record_initial_provider_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
) -> Result<()> {
    record_provider_failure_for_claim_condition(
        directory,
        provider,
        error,
        provider_session_id,
        turn_id,
        None,
        true,
    )
}

fn record_provider_failure_for_claim_condition(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    expected_claim_token: Option<&str>,
    require_no_prior_provider_event: bool,
) -> Result<()> {
    let expected_claim_token = match expected_claim_token {
        Some(token) => token.to_owned(),
        None => match current_turn_claim_token(directory)? {
            Some(token) => token,
            None => return Ok(()),
        },
    };
    let expected_claim_token = Some(expected_claim_token.as_str());
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _claim_lock = lock_turn_claim(&claim_path)?;
    recover_pending_completion_locked(directory, &claim_path)?;
    if require_no_prior_provider_event && provider_has_completed_turn(directory, provider)? {
        return Ok(());
    }
    if !provider_completion_is_current(
        directory,
        provider,
        provider_session_id.as_deref(),
        turn_id.as_deref(),
        expected_claim_token,
    )? {
        return Ok(());
    }
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id,
        turn_id,
        created_unix_ms: unix_ms(),
    };
    commit_provider_completion_locked(
        directory,
        &claim_path,
        expected_claim_token.context("provider completion has no claim token")?,
        event,
        Some(error),
    )
}

fn record_provider_monitor_failure(
    directory: &Path,
    provider: FirstPartyCli,
    error: &str,
) -> Result<()> {
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _claim_lock = lock_turn_claim(&claim_path)?;
    recover_pending_completion_locked(directory, &claim_path)?;
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if matches!(status.state.as_str(), "closed" | "exited" | "failed") {
        return Ok(());
    }
    let Some(claim_token) = current_turn_claim_token(directory)? else {
        return update_status(
            directory,
            "failed",
            None,
            Some(terminal_safe_text(error, true)),
        );
    };
    let error = terminal_safe_text(error, true);
    let event = SessionEvent {
        provider: provider.as_str().to_owned(),
        message: String::new(),
        error: Some(error.clone()),
        provider_session_id: None,
        turn_id: None,
        created_unix_ms: unix_ms(),
    };
    commit_provider_completion_with_status_locked(
        directory,
        &claim_path,
        &claim_token,
        event,
        Some(error),
        "failed",
    )
}

fn provider_has_completed_turn(directory: &Path, provider: FirstPartyCli) -> Result<bool> {
    for path in event_paths(directory)? {
        let event: SessionEvent = read_json(&path)?;
        if event.provider == provider.as_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn current_turn_claim_token(directory: &Path) -> Result<Option<String>> {
    match fs::read_to_string(directory.join(TURN_CLAIM_FILE)) {
        Ok(token) => Ok(Some(token.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to inspect native turn claim"),
    }
}

fn provider_completion_is_current(
    directory: &Path,
    provider: FirstPartyCli,
    provider_session_id: Option<&str>,
    turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
) -> Result<bool> {
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(
        status.state.as_str(),
        "running" | "working" | "resume-pending"
    ) {
        return Ok(false);
    }
    if let Some(expected_claim_token) = expected_claim_token {
        let current = match fs::read_to_string(directory.join(TURN_CLAIM_FILE)) {
            Ok(current) => current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to inspect native turn claim"),
        };
        if current.trim() != expected_claim_token {
            return Ok(false);
        }
    }
    let Some(turn_id) = turn_id else {
        return Ok(true);
    };
    for path in event_paths(directory)? {
        let event: SessionEvent = read_json(&path)?;
        if event.provider == provider.as_str()
            && event.provider_session_id.as_deref() == provider_session_id
            && event.turn_id.as_deref() == Some(turn_id)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn commit_provider_completion_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
) -> Result<()> {
    commit_provider_completion_with_status_locked(
        directory,
        claim_path,
        claim_token,
        event,
        status_error,
        "ready",
    )
}

fn commit_provider_completion_with_status_locked(
    directory: &Path,
    claim_path: &Path,
    claim_token: &str,
    event: SessionEvent,
    status_error: Option<String>,
    status_state: &str,
) -> Result<()> {
    let mut pending =
        PendingTurnCompletion::new_with_status(claim_token, event, status_error, status_state)?;
    // Request indexing must not prevent a provider-verified completion from publishing.
    // A missing or damaged receipt remains explicitly unresolved in request queries.
    if let Ok(Some(receipt)) = requests::for_claim(directory, claim_token) {
        pending.event_file = receipt.event_file;
    }
    write_private(
        &directory.join(TURN_COMPLETION_FILE),
        &serde_json::to_vec_pretty(&pending)?,
    )?;
    recover_pending_completion_locked(directory, claim_path)?;
    Ok(())
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
    temporary.as_file().sync_all()?;
    let persisted = temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist {}", path.display()))?;
    persisted.sync_all()?;
    sync_parent_directory(path)?;
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
    file.sync_all()?;
    sync_parent_directory(path)?;
    Ok(())
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    sync_directory(parent)
        .with_context(|| format!("failed to sync state directory {}", parent.display()))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?
        .sync_all()?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_directory: &Path) -> Result<()> {
    Ok(())
}

fn update_status(
    directory: &Path,
    state: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    let _status_lock = lock_status(directory)?;
    update_status_locked(directory, state, exit_code, error)
}

struct StatusLock {
    _file: File,
}

fn lock_status(directory: &Path) -> Result<StatusLock> {
    let path = directory.join(STATUS_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    set_private_file_permissions(&file)?;
    file.lock()
        .with_context(|| "failed to lock native session status")?;
    Ok(StatusLock { _file: file })
}

fn update_status_locked(
    directory: &Path,
    state: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    let status_path = directory.join("status.json");
    let closed_path = directory.join(CLOSED_STATUS_FILE);
    if let Some(closed) = read_status_if_present(&closed_path)? {
        if state != "closed" {
            return write_json_atomic(&status_path, &closed);
        }
        return write_json_atomic(&status_path, &closed);
    }
    let current = read_status_if_present(&status_path)?;
    if let Some(current) = &current
        && !valid_status_transition(&current.state, state)
    {
        bail!(
            "invalid native session status transition {} -> {state}",
            current.state
        )
    }
    let generation = current
        .as_ref()
        .map_or(0, |status| status.generation)
        .checked_add(1)
        .context("native session status generation overflowed")?;
    let status = SessionStatus {
        state: state.to_owned(),
        generation,
        updated_unix_ms: unix_ms(),
        exit_code,
        error,
    };
    if state == "closed" {
        write_json_atomic(&closed_path, &status)?;
        return write_json_atomic(&status_path, &status);
    }
    write_json_atomic(&status_path, &status)
}

fn valid_status_transition(current: &str, next: &str) -> bool {
    current == next
        || matches!(
            (current, next),
            (
                "launching",
                "running" | "awaiting-initial-input" | "failed" | "closed"
            ) | (
                "awaiting-initial-input",
                "working" | "exited" | "failed" | "closed"
            ) | ("running", "ready" | "exited" | "failed" | "closed")
                | ("ready", "claimed" | "exited" | "failed" | "closed")
                | (
                    "claimed",
                    "working" | "ready" | "exited" | "failed" | "closed"
                )
                | ("working", "ready" | "exited" | "failed" | "closed")
                | (
                    "resume-pending",
                    "working" | "ready" | "exited" | "failed" | "closed"
                )
                | ("exited" | "failed", "closed")
                | ("closed", "closed")
        )
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
    receipt: requests::Receipt,
    retained: bool,
    rollback_state: Option<&'static str>,
}

struct TurnClaimLock {
    _file: File,
}

fn lock_turn_claim(path: &Path) -> Result<TurnClaimLock> {
    let lock_path = path.with_file_name(TURN_CLAIM_LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    set_private_file_permissions(&file)?;
    file.lock()
        .with_context(|| "failed to lock native turn claim lifecycle")?;
    Ok(TurnClaimLock { _file: file })
}

impl TurnClaim {
    fn retain_in_place(&mut self) {
        self.retained = true;
    }

    fn retain(mut self) {
        self.retain_in_place();
    }
}

fn record_initial_prompt_delivery_failure(
    directory: &Path,
    claim: &mut TurnClaim,
    delivery_started: bool,
    error: &anyhow::Error,
) {
    let error = terminal_safe_text(&format!("{error:#}"), true);
    if delivery_started {
        claim.retain_in_place();
        let _ = update_status(directory, "working", None, Some(error));
    } else {
        let _ = update_status(directory, "failed", None, Some(error));
    }
}

fn record_follow_up_terminal_delivery_failure(
    directory: &Path,
    claim: &mut TurnClaim,
    failure: &terminal::TerminalSendFailure,
) {
    if failure.delivery_may_have_occurred() {
        claim.retain_in_place();
        let error = terminal_safe_text(&format!("{:#}", failure.error()), true);
        let _ = update_status(directory, "working", None, Some(error));
    }
}

// A turn that stays claimed looks like ordinary work from the state alone, so the status
// keeps the reason until the target completes the turn or the session is closed. The target
// can complete a delivered turn before its sender stops settling; the session status then
// belongs to whichever turn holds the claim now, not to this report.
fn record_follow_up_cross_session_delivery_uncertainty(
    directory: &Path,
    claim: &mut TurnClaim,
    error: &anyhow::Error,
) {
    claim.retain_in_place();
    let Ok(_lock) = lock_turn_claim(&claim.path) else {
        return;
    };
    if !matches!(current_turn_claim_token(directory), Ok(Some(token)) if token == claim.token) {
        return;
    }
    let error = terminal_safe_text(&format!("{error:#}"), true);
    let _ = update_status(directory, "working", None, Some(error));
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if !self.retained {
            if let Some(state) = self.rollback_state {
                let _ = rollback_turn_claim_token(&self.path, &self.token, state);
            } else {
                let _ = release_turn_claim_token(&self.path, &self.token);
            }
        }
    }
}

fn rollback_turn_claim_token(path: &Path, expected_token: &str, state: &str) -> Result<()> {
    let _lock = lock_turn_claim(path)?;
    let current = match fs::read_to_string(path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect native turn claim"),
    };
    if current.trim() != expected_token {
        return Ok(());
    }
    remove_turn_claim_locked(path)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    update_status(directory, state, None, None)
}

fn acquire_turn_claim(directory: &Path) -> Result<TurnClaim> {
    let path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&path)?;
    create_turn_claim_locked(path)
}

fn acquire_ready_turn_claim(directory: &Path, session_id: &str) -> Result<(TurnClaim, usize)> {
    acquire_ready_turn_claim_after_claim(directory, session_id, || Ok(()))
}

fn acquire_ready_turn_claim_after_claim<F>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
) -> Result<(TurnClaim, usize)>
where
    F: FnOnce() -> Result<()>,
{
    acquire_ready_turn_claim_with_callbacks(directory, session_id, after_claim, || {})
}

fn acquire_ready_turn_claim_with_callbacks<F, G>(
    directory: &Path,
    session_id: &str,
    after_claim: F,
    before_publish: G,
) -> Result<(TurnClaim, usize)>
where
    F: FnOnce() -> Result<()>,
    G: FnOnce(),
{
    let path = directory.join(TURN_CLAIM_FILE);
    let state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&state) {
        bail!("session {session_id} is {state}; tell requires the ready state");
    }
    let mut claim = {
        let _lock = lock_turn_claim(&path)?;
        let state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
        if !session_accepts_prompt(&state) {
            bail!("session {session_id} is {state}; tell requires the ready state");
        }
        create_turn_claim_locked(path.clone())?
    };
    if let Err(error) = after_claim() {
        let _lock = lock_turn_claim(&path)?;
        let _ = release_turn_claim_token_locked(&path, &claim.token);
        claim.retain();
        return Err(error);
    }
    let _lock = lock_turn_claim(&path)?;
    let current = fs::read_to_string(&path)
        .with_context(|| "native turn claim disappeared before it could start")?;
    if current.trim() != claim.token {
        bail!("native turn claim changed before it could start");
    }
    let state = read_json::<SessionStatus>(&directory.join("status.json"))?.state;
    if !session_accepts_prompt(&state) {
        bail!("session {session_id} is {state}; tell requires the ready state");
    }
    let baseline = event_paths(directory)?.len();
    before_publish();
    if let Err(error) = update_status(directory, "claimed", None, None) {
        let _ = release_turn_claim_token_locked(&path, &claim.token);
        claim.retain();
        return Err(error);
    }
    claim.rollback_state = Some("ready");
    Ok((claim, baseline))
}

fn create_turn_claim_locked(path: PathBuf) -> Result<TurnClaim> {
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
    file.sync_all()?;
    sync_parent_directory(&path)?;
    let directory = path
        .parent()
        .context("turn claim has no session directory")?;
    let receipt = match requests::create(directory, &token) {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = remove_turn_claim_locked(&path);
            return Err(error).context("failed to persist request receipt before dispatch");
        }
    };
    Ok(TurnClaim {
        path,
        token,
        receipt,
        retained: false,
        rollback_state: None,
    })
}

fn release_turn_claim_token(path: &Path, expected_token: &str) -> Result<()> {
    let _lock = lock_turn_claim(path)?;
    release_turn_claim_token_locked(path, expected_token)
}

fn release_turn_claim_token_locked(path: &Path, expected_token: &str) -> Result<()> {
    let token = match fs::read_to_string(path) {
        Ok(token) => token,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect native turn claim"),
    };
    if token.trim() != expected_token {
        return Ok(());
    }
    remove_file_if_present(path).context("failed to release native turn claim")
}

#[cfg(test)]
fn release_turn_claim(directory: &Path) -> Result<()> {
    let path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&path)?;
    remove_turn_claim_locked(&path)
}

fn remove_turn_claim_locked(path: &Path) -> Result<()> {
    remove_file_if_present(path).context("failed to release native turn claim")
}

fn valid_turn_claim_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 160
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}

fn recover_pending_completion(directory: &Path) -> Result<bool> {
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&claim_path)?;
    recover_pending_completion_locked(directory, &claim_path)
}

fn recover_pending_completion_locked(directory: &Path, claim_path: &Path) -> Result<bool> {
    let completion_path = directory.join(TURN_COMPLETION_FILE);
    let Some(text) = read_regular_text_if_present(&completion_path)? else {
        return Ok(false);
    };
    let pending: PendingTurnCompletion =
        serde_json::from_str(&text).context("invalid pending native turn completion")?;
    validate_pending_completion(&pending)?;
    if directory.join(CLOSED_STATUS_FILE).is_file() {
        remove_file_if_present(&completion_path)?;
        return Ok(true);
    }

    match fs::read_to_string(claim_path) {
        Ok(current) if current.trim() == pending.claim_token => {
            write_pending_completion_event(directory, &pending)?;
            update_status(
                directory,
                &pending.status_state,
                None,
                pending.status_error.clone(),
            )?;
            release_turn_claim_token_locked(claim_path, &pending.claim_token)?;
            remove_file_if_present(&completion_path)?;
            Ok(true)
        }
        Ok(_) => bail!("pending native completion belongs to a different turn claim"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let event_path = directory.join("events").join(&pending.event_file);
            let stored: SessionEvent = read_json(&event_path)
                .context("claim-free pending completion has no committed event")?;
            if stored != pending.event {
                bail!("claim-free pending completion event does not match its journal")
            }
            let status: SessionStatus = read_json(&directory.join("status.json"))?;
            if status.state != pending.status_state || status.error != pending.status_error {
                bail!("claim-free pending completion has no matching terminal status")
            }
            remove_file_if_present(&completion_path)?;
            Ok(true)
        }
        Err(error) => Err(error).context("failed to inspect pending completion turn claim"),
    }
}

fn validate_pending_completion(pending: &PendingTurnCompletion) -> Result<()> {
    if pending.schema != 1
        || !valid_turn_claim_token(&pending.claim_token)
        || !matches!(pending.status_state.as_str(), "ready" | "failed")
    {
        bail!("invalid pending native completion identity")
    }
    if !valid_event_file_name(&pending.event_file) {
        bail!("invalid pending native completion event file")
    }
    if pending.event.error != pending.status_error {
        bail!("pending native completion status does not match its event")
    }
    Ok(())
}

fn write_pending_completion_event(directory: &Path, pending: &PendingTurnCompletion) -> Result<()> {
    validate_pending_completion(pending)?;
    let path = directory.join("events").join(&pending.event_file);
    if path.exists() {
        let stored: SessionEvent = read_json(&path)?;
        if stored != pending.event {
            bail!("pending native completion event file contains different data")
        }
        return Ok(());
    }
    write_json_atomic(&path, &pending.event)
}

fn mark_session_closed(directory: &Path, error: Option<String>) -> Result<()> {
    let claim_path = directory.join(TURN_CLAIM_FILE);
    let _lock = lock_turn_claim(&claim_path)?;
    mark_session_closed_locked(directory, &claim_path, error)
}

fn mark_session_closed_locked(
    directory: &Path,
    claim_path: &Path,
    error: Option<String>,
) -> Result<()> {
    let consume_result = if directory.join(TERMINAL_HANDLE_FILE).exists()
        || directory.join(TERMINAL_CLOSING_FILE).exists()
    {
        consume_terminal_handle(directory, None)
    } else {
        Ok(())
    };
    let status_result = update_status(directory, "closed", None, error);
    let (pending_result, running_result, claim_result) = if status_result.is_ok() {
        (
            remove_file_if_present(&directory.join(TURN_COMPLETION_FILE))
                .and_then(|_| remove_file_if_present(&directory.join(LEGACY_RESUME_PENDING_FILE))),
            remove_file_if_present(&directory.join(LEGACY_RESUME_RUNNING_FILE)),
            remove_turn_claim_locked(claim_path),
        )
    } else {
        (Ok(()), Ok(()), Ok(()))
    };
    consume_result?;
    status_result?;
    pending_result?;
    running_result?;
    claim_result
}

fn repair_dead_native_owner(directory: &Path) -> Result<bool> {
    repair_dead_native_owner_with_terminal_close(directory, terminal::close_session)
}

fn repair_dead_native_owner_with_terminal_close<F>(
    directory: &Path,
    mut close_terminal: F,
) -> Result<bool>
where
    F: FnMut(&terminal::TerminalSession) -> Result<terminal::CloseOutcome>,
{
    #[cfg(not(windows))]
    let _ = &mut close_terminal;
    recover_pending_completion(directory)?;
    let status: SessionStatus = read_json(&directory.join("status.json"))?;
    if !matches!(
        status.state.as_str(),
        "launching"
            | "awaiting-initial-input"
            | "running"
            | "ready"
            | "claimed"
            | "resume-pending"
            | "working"
            | "exited"
            | "failed"
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
    #[cfg(target_os = "macos")]
    if mac_native_owner_is_live(&owner)? {
        return Ok(false);
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
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
        close_session_state_with_error(directory, repair_error, |session| {
            if session.kind != terminal::TerminalKind::WindowsConsole {
                bail!("dead Windows native owner has a non-Windows terminal handle")
            }
            close_terminal(session)
        })
        .context("failed to close a Windows console whose native owner exited")?;
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

#[cfg(test)]
fn write_event(directory: &Path, event: &SessionEvent) -> Result<()> {
    write_json_atomic(
        &directory.join("events").join(new_event_file_name()?),
        event,
    )
}

fn new_event_file_name() -> Result<String> {
    Ok(format!(
        "event-{}-{}.json",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id()
    ))
}

fn valid_event_file_name(name: &str) -> bool {
    name.starts_with("event-")
        && name.ends_with(".json")
        && name.len() <= 128
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
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
                .is_some_and(valid_event_file_name)
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn wait_for_status(
    directory: &Path,
    expected_state: &str,
    deadline: Instant,
    requested: Duration,
) -> Result<SessionStatus> {
    loop {
        recover_pending_completion(directory)?;
        repair_dead_native_owner(directory)?;
        if let Ok(status) = read_json::<SessionStatus>(&directory.join("status.json")) {
            if status.state == expected_state {
                return Ok(status);
            }
            if matches!(status.state.as_str(), "failed" | "exited" | "closed") {
                let reason = status
                    .error
                    .unwrap_or_else(|| format!("session entered state {}", status.state));
                bail!("{reason}");
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .with_context(|| {
                format!(
                    "timed out after {} seconds waiting for session state {expected_state}",
                    requested.as_secs()
                )
            })?;
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
fn wait_for_event(directory: &Path, baseline: usize, timeout: Duration) -> Result<SessionEvent> {
    wait_for_event_for_turn(directory, baseline, None, None, timeout)
}

#[cfg(test)]
fn wait_for_event_for_turn(
    directory: &Path,
    baseline: usize,
    expected_turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
    timeout: Duration,
) -> Result<SessionEvent> {
    let deadline = checked_deadline_from(Instant::now(), timeout)?;
    wait_for_event_for_turn_until(
        directory,
        baseline,
        expected_turn_id,
        expected_claim_token,
        deadline,
        timeout,
    )
}

fn wait_for_event_for_turn_until(
    directory: &Path,
    baseline: usize,
    expected_turn_id: Option<&str>,
    expected_claim_token: Option<&str>,
    deadline: Instant,
    requested: Duration,
) -> Result<SessionEvent> {
    loop {
        recover_pending_completion(directory)?;
        repair_dead_native_owner(directory)?;
        let paths = event_paths(directory)?;
        if paths.len() > baseline {
            let candidates = &paths[baseline..];
            let event = if let Some(expected_turn_id) = expected_turn_id {
                let mut matched = None;
                for path in candidates {
                    let event: SessionEvent = read_json(path)?;
                    if event.turn_id.as_deref() == Some(expected_turn_id) {
                        matched = Some(event);
                        break;
                    }
                }
                matched
            } else {
                Some(read_json(
                    candidates.first().context("event path disappeared")?,
                )?)
            };
            if let Some(event) = event
                && turn_completion_was_published(directory, expected_claim_token)?
            {
                if let Some(error) = event.error.as_deref() {
                    bail!("{error}");
                }
                return Ok(event);
            }
        }
        if let Ok(status) = read_json::<SessionStatus>(&directory.join("status.json"))
            && matches!(status.state.as_str(), "failed" | "exited" | "closed")
        {
            let reason = status
                .error
                .unwrap_or_else(|| format!("session entered state {}", status.state));
            bail!("{reason}");
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .with_context(|| format!("timed out after {} seconds", requested.as_secs()))?;
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

fn turn_completion_was_published(
    directory: &Path,
    expected_claim_token: Option<&str>,
) -> Result<bool> {
    if let Some(expected_token) = expected_claim_token {
        return match fs::read_to_string(directory.join(TURN_CLAIM_FILE)) {
            Ok(current_token) => Ok(current_token.trim() != expected_token),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error).context("failed to inspect native turn claim"),
        };
    }
    Ok(read_json::<SessionStatus>(&directory.join("status.json"))
        .is_ok_and(|status| status.state == "ready"))
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
    check_provider_version_until(provider, executable, None)
}

fn check_provider_version_until(
    provider: FirstPartyCli,
    executable: &Path,
    deadline: Option<Instant>,
) -> Result<String> {
    let mut command = provider_version_command(executable)?;
    command.arg("--version");
    let label = format!("{} --version", executable.display());
    let output = match deadline {
        Some(deadline) => command_output_until(&mut command, deadline, &label),
        None => command
            .output()
            .with_context(|| format!("failed to query {label}")),
    }?;
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
        "cd {} && {}={} {} native-session {}; bridge_status=$?; exit \"$bridge_status\"",
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
        "Set-Location -LiteralPath {} -ErrorAction Stop; $env:{} = {}; & {} native-session {}; exit $LASTEXITCODE",
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

fn terminal_input_bytes(kind: terminal::TerminalKind, prompt: &str) -> Vec<u8> {
    if matches!(
        kind,
        terminal::TerminalKind::Ghostty | terminal::TerminalKind::WindowsConsole
    ) {
        // Ghostty's `input text` command already delivers its argument as a paste.
        // WriteConsoleInputW emits key events rather than a terminal paste, so Windows
        // CLIs also expose a bracketed-paste envelope as literal `[200~...` text.
        prompt.as_bytes().to_vec()
    } else {
        format!("\x1b[200~{prompt}\x1b[201~").into_bytes()
    }
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
